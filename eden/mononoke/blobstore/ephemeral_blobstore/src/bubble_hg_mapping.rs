/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use bonsai_hg_mapping::BonsaiOrHgChangesetIds;
use context::CoreContext;
use futures::future;
use mercurial_types::HgChangesetId;
use mononoke_types::RepositoryId;
use stats::prelude::*;

use crate::bubble::BubbleId;
use crate::error::EphemeralBlobstoreError;
use crate::store::SqlBubbleHgMapping;
use crate::store::leftover_ids;

const ENABLE_BUBBLE_HG_MAPPING: &str = "scm/mononoke:enable_bubble_hg_mapping";

define_stats! {
    prefix = "mononoke.ephemeral_blobstore.hg_mapping";
    wrapper_bubble_fallbacks: timeseries(Sum),
    wrapper_permanent_fallbacks: timeseries(Sum),
}

pub struct BubbleBonsaiHgMapping {
    persistent: Arc<dyn BonsaiHgMapping>,
    bubble: SqlBubbleHgMapping,
    bubble_id: BubbleId,
    repo_name: String,
}

impl BubbleBonsaiHgMapping {
    pub(crate) fn new(
        persistent: Arc<dyn BonsaiHgMapping>,
        bubble: SqlBubbleHgMapping,
        bubble_id: BubbleId,
        repo_name: String,
    ) -> Self {
        Self {
            persistent,
            bubble,
            bubble_id,
            repo_name,
        }
    }

    fn enabled(&self) -> bool {
        justknobs::eval(ENABLE_BUBBLE_HG_MAPPING, None, Some(&self.repo_name))
    }

    async fn get_bubble_first(
        &self,
        ctx: &CoreContext,
        ids: BonsaiOrHgChangesetIds,
    ) -> Result<Vec<BonsaiHgMappingEntry>> {
        let from_bubble = self.bubble.get(ctx, self.bubble_id, ids.clone()).await?;
        let leftovers = leftover_ids(ids, &from_bubble);
        if leftovers.is_empty() {
            return Ok(from_bubble);
        }

        STATS::wrapper_permanent_fallbacks.add_value(1);
        let from_permanent = self.persistent.get(ctx, leftovers).await?;
        Ok(from_bubble.into_iter().chain(from_permanent).collect())
    }

    async fn get_permanent_first(
        &self,
        ctx: &CoreContext,
        ids: BonsaiOrHgChangesetIds,
    ) -> Result<Vec<BonsaiHgMappingEntry>> {
        let from_permanent = self.persistent.get(ctx, ids.clone()).await?;
        let leftovers = leftover_ids(ids, &from_permanent);
        if leftovers.is_empty() {
            return Ok(from_permanent);
        }

        STATS::wrapper_bubble_fallbacks.add_value(1);
        let from_bubble = self.bubble.get(ctx, self.bubble_id, leftovers).await?;
        Ok(from_permanent.into_iter().chain(from_bubble).collect())
    }
}

#[async_trait]
impl BonsaiHgMapping for BubbleBonsaiHgMapping {
    fn repo_id(&self) -> RepositoryId {
        self.persistent.repo_id()
    }

    async fn add(&self, ctx: &CoreContext, entry: BonsaiHgMappingEntry) -> Result<bool> {
        self.bulk_add(ctx, &[entry]).await.map(|rows| rows >= 1)
    }

    async fn bulk_add(&self, ctx: &CoreContext, entries: &[BonsaiHgMappingEntry]) -> Result<u64> {
        if !self.enabled() {
            return Err(EphemeralBlobstoreError::BubbleHgMappingDisabled.into());
        }
        self.bubble.add(ctx, self.bubble_id, entries).await
    }

    async fn get(
        &self,
        ctx: &CoreContext,
        ids: BonsaiOrHgChangesetIds,
    ) -> Result<Vec<BonsaiHgMappingEntry>> {
        if !self.enabled() {
            return self.persistent.get(ctx, ids).await;
        }
        match ids {
            BonsaiOrHgChangesetIds::Bonsai(_) => self.get_bubble_first(ctx, ids).await,
            BonsaiOrHgChangesetIds::Hg(_) => self.get_permanent_first(ctx, ids).await,
        }
    }

    async fn get_hg_in_range(
        &self,
        ctx: &CoreContext,
        low: HgChangesetId,
        high: HgChangesetId,
        limit: usize,
    ) -> Result<Vec<HgChangesetId>> {
        if !self.enabled() {
            return self.persistent.get_hg_in_range(ctx, low, high, limit).await;
        }
        let (from_permanent, from_bubble) = future::try_join(
            self.persistent.get_hg_in_range(ctx, low, high, limit),
            self.bubble
                .get_hg_in_range(ctx, self.bubble_id, low, high, limit),
        )
        .await?;
        Ok(from_permanent
            .into_iter()
            .chain(from_bubble)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(limit)
            .collect())
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::slice;
    use std::str::FromStr;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use blobstore::BlobstoreEnumerableWithUnlink;
    use bonsai_hg_mapping::CachingBonsaiHgMapping;
    use bonsai_hg_mapping::SqlBonsaiHgMappingBuilder;
    use fbinit::FacebookInit;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use memblob::Memblob;
    use mercurial_types::HgChangesetIdPrefix;
    use mercurial_types::HgChangesetIdsResolvedFromPrefix;
    use mercurial_types_mocks::nodehash as hg;
    use metaconfig_types::BubbleDeletionMode;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types_mocks::changesetid as bonsai;
    use mononoke_types_mocks::repo::REPO_ZERO;
    use rendezvous::RendezVousOptions;
    use sql_construct::SqlConstruct;

    use super::*;
    use crate::builder::RepoEphemeralStoreBuilder;

    struct Fixture {
        ctx: CoreContext,
        persistent: Arc<dyn BonsaiHgMapping>,
        bubble_store: SqlBubbleHgMapping,
        bubble_id: BubbleId,
        wrapper: Arc<dyn BonsaiHgMapping>,
    }

    async fn fixture(fb: FacebookInit, persistent: Arc<dyn BonsaiHgMapping>) -> Result<Fixture> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = Arc::new(Memblob::default()) as Arc<dyn BlobstoreEnumerableWithUnlink>;
        let eph = RepoEphemeralStoreBuilder::with_sqlite_in_memory()?.build(
            REPO_ZERO,
            blobstore,
            Duration::from_hours(24),
            Duration::from_hours(6),
            BubbleDeletionMode::MarkAndDelete,
        );
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let wrapper = bubble.bonsai_hg_mapping(REPO_ZERO, "repo", persistent.clone());
        Ok(Fixture {
            ctx,
            persistent,
            bubble_store: SqlBubbleHgMapping::new(bubble.connections.clone(), REPO_ZERO),
            bubble_id: bubble.bubble_id(),
            wrapper,
        })
    }

    fn sql_persistent() -> Result<Arc<dyn BonsaiHgMapping>> {
        Ok(Arc::new(
            SqlBonsaiHgMappingBuilder::with_sqlite_in_memory()?
                .build(REPO_ZERO, RendezVousOptions::for_test()),
        ))
    }

    fn entry(hg_cs_id: HgChangesetId, bcs_id: ChangesetId) -> BonsaiHgMappingEntry {
        BonsaiHgMappingEntry { hg_cs_id, bcs_id }
    }

    fn prefixed(suffix: &str) -> Result<HgChangesetId> {
        HgChangesetId::from_str(&format!("aaaa{suffix:0>36}"))
    }

    async fn add_to_bubble(f: &Fixture, entries: &[BonsaiHgMappingEntry]) -> Result<()> {
        f.bubble_store.add(&f.ctx, f.bubble_id, entries).await?;
        Ok(())
    }

    async fn with_knob<T>(
        on: bool,
        fut: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        with_just_knobs_async(
            JustKnobsInMemory::new(HashMap::from([(
                ENABLE_BUBBLE_HG_MAPPING.to_string(),
                KnobVal::Bool(on),
            )])),
            fut.boxed(),
        )
        .await
    }

    struct CountingBonsaiHgMapping {
        inner: Arc<dyn BonsaiHgMapping>,
        gets: Arc<AtomicUsize>,
        adds: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl BonsaiHgMapping for CountingBonsaiHgMapping {
        fn repo_id(&self) -> RepositoryId {
            self.inner.repo_id()
        }

        async fn add(&self, ctx: &CoreContext, entry: BonsaiHgMappingEntry) -> Result<bool> {
            self.adds.fetch_add(1, Ordering::Relaxed);
            self.inner.add(ctx, entry).await
        }

        async fn bulk_add(
            &self,
            ctx: &CoreContext,
            entries: &[BonsaiHgMappingEntry],
        ) -> Result<u64> {
            self.adds.fetch_add(entries.len(), Ordering::Relaxed);
            self.inner.bulk_add(ctx, entries).await
        }

        async fn get(
            &self,
            ctx: &CoreContext,
            ids: BonsaiOrHgChangesetIds,
        ) -> Result<Vec<BonsaiHgMappingEntry>> {
            self.gets.fetch_add(1, Ordering::Relaxed);
            self.inner.get(ctx, ids).await
        }

        async fn get_hg_in_range(
            &self,
            ctx: &CoreContext,
            low: HgChangesetId,
            high: HgChangesetId,
            limit: usize,
        ) -> Result<Vec<HgChangesetId>> {
            self.gets.fetch_add(1, Ordering::Relaxed);
            self.inner.get_hg_in_range(ctx, low, high, limit).await
        }
    }

    #[mononoke::fbinit_test]
    async fn permanent_wins_test(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let f = fixture(fb, sql_persistent()?).await?;
            let ones = entry(hg::ONES_CSID, bonsai::ONES_CSID);
            f.persistent.add(&f.ctx, ones.clone()).await?;
            add_to_bubble(&f, slice::from_ref(&ones)).await?;

            assert_eq!(
                f.wrapper.get(&f.ctx, hg::ONES_CSID.into()).await?,
                vec![ones],
                "an hg id in both stores is returned once"
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn bubble_leftovers_test(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let f = fixture(fb, sql_persistent()?).await?;
            let ones = entry(hg::ONES_CSID, bonsai::ONES_CSID);
            let twos = entry(hg::TWOS_CSID, bonsai::TWOS_CSID);
            f.persistent.add(&f.ctx, ones.clone()).await?;
            add_to_bubble(&f, slice::from_ref(&twos)).await?;

            let found = f
                .wrapper
                .get(&f.ctx, vec![hg::ONES_CSID, hg::TWOS_CSID].into())
                .await?;

            assert_eq!(
                found.into_iter().collect::<HashSet<_>>(),
                HashSet::from([ones, twos])
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn prefix_merges_and_dedupes_test(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let f = fixture(fb, sql_persistent()?).await?;
            let shared = entry(prefixed("01")?, bonsai::ONES_CSID);
            f.persistent.add(&f.ctx, shared.clone()).await?;
            add_to_bubble(&f, slice::from_ref(&shared)).await?;

            let resolved = f
                .wrapper
                .get_many_hg_by_prefix(&f.ctx, HgChangesetIdPrefix::from_str("aaaa")?, 10)
                .await?;

            assert_eq!(
                resolved,
                HgChangesetIdsResolvedFromPrefix::Single(shared.hg_cs_id)
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn prefix_too_many_across_stores_test(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let f = fixture(fb, sql_persistent()?).await?;
            f.persistent
                .bulk_add(
                    &f.ctx,
                    &[
                        entry(prefixed("01")?, bonsai::ONES_CSID),
                        entry(prefixed("02")?, bonsai::TWOS_CSID),
                    ],
                )
                .await?;
            add_to_bubble(
                &f,
                &[
                    entry(prefixed("03")?, bonsai::THREES_CSID),
                    entry(prefixed("04")?, bonsai::FOURS_CSID),
                ],
            )
            .await?;

            let resolved = f
                .wrapper
                .get_many_hg_by_prefix(&f.ctx, HgChangesetIdPrefix::from_str("aaaa")?, 3)
                .await?;

            assert!(
                matches!(resolved, HgChangesetIdsResolvedFromPrefix::TooMany(_)),
                "four matches across both stores exceed a limit of three: {resolved:?}"
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn writes_go_to_bubble_only_test(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let f = fixture(fb, sql_persistent()?).await?;
            let threes = entry(hg::THREES_CSID, bonsai::THREES_CSID);

            assert!(f.wrapper.add(&f.ctx, threes.clone()).await?);

            assert!(
                f.persistent
                    .get(&f.ctx, hg::THREES_CSID.into())
                    .await?
                    .is_empty(),
                "the permanent mapping must not get bubble writes"
            );
            assert_eq!(
                f.wrapper.get(&f.ctx, hg::THREES_CSID.into()).await?,
                vec![threes]
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn bubble_rows_never_cached_test(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let caching: Arc<dyn BonsaiHgMapping> =
                Arc::new(CachingBonsaiHgMapping::new_test(sql_persistent()?));
            let f = fixture(fb, caching.clone()).await?;
            let fours = entry(hg::FOURS_CSID, bonsai::FOURS_CSID);
            f.wrapper.add(&f.ctx, fours.clone()).await?;

            assert_eq!(
                f.wrapper.get(&f.ctx, hg::FOURS_CSID.into()).await?,
                vec![fours.clone()]
            );
            assert_eq!(
                f.wrapper.get(&f.ctx, hg::FOURS_CSID.into()).await?,
                vec![fours]
            );
            assert!(
                caching.get(&f.ctx, hg::FOURS_CSID.into()).await?.is_empty(),
                "the cached permanent mapping must never see bubble rows"
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn knob_off_reads_permanent_only_test(fb: FacebookInit) -> Result<()> {
        with_knob(false, async move {
            let f = fixture(fb, sql_persistent()?).await?;
            add_to_bubble(&f, &[entry(hg::ONES_CSID, bonsai::ONES_CSID)]).await?;

            assert!(
                f.wrapper
                    .get(&f.ctx, hg::ONES_CSID.into())
                    .await?
                    .is_empty()
            );
            assert!(
                f.wrapper
                    .get(&f.ctx, bonsai::ONES_CSID.into())
                    .await?
                    .is_empty()
            );
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn knob_off_add_fails_test(fb: FacebookInit) -> Result<()> {
        with_knob(false, async move {
            let f = fixture(fb, sql_persistent()?).await?;

            let err = f
                .wrapper
                .add(&f.ctx, entry(hg::ONES_CSID, bonsai::ONES_CSID))
                .await
                .expect_err("writes must fail with the knob off");

            assert!(matches!(
                err.downcast_ref::<EphemeralBlobstoreError>(),
                Some(EphemeralBlobstoreError::BubbleHgMappingDisabled)
            ));
            Ok(())
        })
        .await
    }

    #[mononoke::fbinit_test]
    async fn bonsai_lookup_bubble_first(fb: FacebookInit) -> Result<()> {
        with_knob(true, async move {
            let gets = Arc::new(AtomicUsize::new(0));
            let adds = Arc::new(AtomicUsize::new(0));
            let counting: Arc<dyn BonsaiHgMapping> = Arc::new(CountingBonsaiHgMapping {
                inner: sql_persistent()?,
                gets: gets.clone(),
                adds: adds.clone(),
            });
            let f = fixture(fb, counting).await?;
            let fives = entry(hg::FIVES_CSID, bonsai::FIVES_CSID);
            f.wrapper.add(&f.ctx, fives.clone()).await?;

            assert_eq!(
                f.wrapper.get(&f.ctx, bonsai::FIVES_CSID.into()).await?,
                vec![fives]
            );
            assert_eq!(
                gets.load(Ordering::Relaxed),
                0,
                "a bonsai found in the bubble must not reach the permanent mapping"
            );
            assert_eq!(adds.load(Ordering::Relaxed), 0);

            assert!(
                f.wrapper
                    .get(&f.ctx, bonsai::ONES_CSID.into())
                    .await?
                    .is_empty()
            );
            assert_eq!(
                gets.load(Ordering::Relaxed),
                1,
                "a bonsai missing from the bubble falls back to the permanent mapping"
            );
            Ok(())
        })
        .await
    }
}
