/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use bonsai_hg_mapping::BonsaiHgMappingError;
use bonsai_hg_mapping::BonsaiOrHgChangesetIds;
use context::CoreContext;
use context::PerfCounterType;
use futures::future;
use mercurial_types::HgChangesetId;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_ext::mononoke_queries;
use stats::prelude::*;

use crate::bubble::BubbleId;
use crate::error::EphemeralBlobstoreError;

define_stats! {
    prefix = "mononoke.ephemeral_blobstore.hg_mapping";
    add_inserted: timeseries(Sum),
    add_conflicts: timeseries(Sum),
    get_master_fallbacks: timeseries(Sum),
}

mononoke_queries! {
    write InsertBubbleHgMapping(values: (
        repo_id: RepositoryId,
        hg_cs_id: HgChangesetId,
        bubble_id: BubbleId,
        bcs_id: ChangesetId,
    )) {
        insert_or_ignore,
        "{insert_or_ignore} INTO ephemeral_bubble_hg_mapping (repo_id, hg_cs_id, bubble_id, bcs_id) VALUES {values}"
    }

    read SelectBubbleHgMappingByHg(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        >list hg_cs_id: HgChangesetId
    ) -> (HgChangesetId, ChangesetId) {
        "SELECT hg_cs_id, bcs_id
         FROM ephemeral_bubble_hg_mapping
         WHERE repo_id = {repo_id}
           AND bubble_id = {bubble_id}
           AND hg_cs_id IN {hg_cs_id}"
    }

    read SelectBubbleHgMappingByBonsai(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        >list bcs_id: ChangesetId
    ) -> (HgChangesetId, ChangesetId) {
        "SELECT hg_cs_id, bcs_id
         FROM ephemeral_bubble_hg_mapping
         WHERE repo_id = {repo_id}
           AND bubble_id = {bubble_id}
           AND bcs_id IN {bcs_id}"
    }

    read SelectBubbleHgChangesetsByRange(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        hg_cs_min: &[u8],
        hg_cs_max: &[u8],
        limit: usize,
    ) -> (HgChangesetId) {
        "SELECT hg_cs_id
         FROM ephemeral_bubble_hg_mapping
         WHERE repo_id = {repo_id}
           AND bubble_id = {bubble_id}
           AND hg_cs_id >= {hg_cs_min} AND hg_cs_id <= {hg_cs_max}
         ORDER BY hg_cs_id
         LIMIT {limit}"
    }

    read SelectAnyBubbleHgMappingByHg(
        repo_id: RepositoryId,
        >list hg_cs_id: HgChangesetId
    ) -> (HgChangesetId, BubbleId, ChangesetId) {
        "SELECT hg_cs_id, bubble_id, bcs_id
         FROM ephemeral_bubble_hg_mapping
         WHERE repo_id = {repo_id}
           AND hg_cs_id IN {hg_cs_id}"
    }

    read SelectAnyBubbleHgMappingByBonsai(
        repo_id: RepositoryId,
        >list bcs_id: ChangesetId
    ) -> (HgChangesetId, BubbleId, ChangesetId) {
        "SELECT hg_cs_id, bubble_id, bcs_id
         FROM ephemeral_bubble_hg_mapping
         WHERE repo_id = {repo_id}
           AND bcs_id IN {bcs_id}"
    }
}

#[derive(Clone)]
pub(crate) struct SqlBubbleHgMapping {
    connections: SqlConnections,
    repo_id: RepositoryId,
}

impl SqlBubbleHgMapping {
    pub(crate) fn new(connections: SqlConnections, repo_id: RepositoryId) -> Self {
        Self {
            connections,
            repo_id,
        }
    }

    pub(crate) async fn add(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        entries: &[BonsaiHgMappingEntry],
    ) -> Result<u64> {
        if entries.is_empty() {
            return Ok(0);
        }

        let rows = entries
            .iter()
            .map(|entry| (&self.repo_id, &entry.hg_cs_id, &bubble_id, &entry.bcs_id))
            .collect::<Vec<_>>();
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        let inserted = InsertBubbleHgMapping::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            rows.as_slice(),
        )
        .await?
        .affected_rows();
        STATS::add_inserted.add_value(inserted as i64);

        if inserted < entries.len() as u64 {
            self.verify(ctx, bubble_id, entries)
                .await
                .inspect_err(|_| STATS::add_conflicts.add_value(1))?;
        }

        Ok(inserted)
    }

    async fn verify(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        entries: &[BonsaiHgMappingEntry],
    ) -> Result<()> {
        let hg_cs_ids = entries
            .iter()
            .map(|entry| entry.hg_cs_id)
            .collect::<Vec<_>>();
        let bcs_ids = entries.iter().map(|entry| entry.bcs_id).collect::<Vec<_>>();

        ctx.perf_counters()
            .add_to_counter(PerfCounterType::SqlReadsMaster, 2);
        let (by_hg, by_bcs) = future::try_join(
            SelectAnyBubbleHgMappingByHg::query(
                &self.connections.read_master_connection,
                ctx.sql_query_telemetry(),
                &self.repo_id,
                hg_cs_ids.as_slice(),
            ),
            SelectAnyBubbleHgMappingByBonsai::query(
                &self.connections.read_master_connection,
                ctx.sql_query_telemetry(),
                &self.repo_id,
                bcs_ids.as_slice(),
            ),
        )
        .await?;

        let by_hg = by_hg
            .into_iter()
            .map(|(hg_cs_id, bubble_id, bcs_id)| (hg_cs_id, (bubble_id, bcs_id)))
            .collect::<HashMap<_, _>>();
        let by_bcs = by_bcs
            .into_iter()
            .map(|(hg_cs_id, _, bcs_id)| (bcs_id, hg_cs_id))
            .collect::<HashMap<_, _>>();

        entries.iter().try_for_each(|entry| {
            if let Some(stored_hg_cs_id) = by_bcs.get(&entry.bcs_id)
                && *stored_hg_cs_id != entry.hg_cs_id
            {
                return Err(BonsaiHgMappingError::ConflictingEntries(
                    BonsaiHgMappingEntry {
                        hg_cs_id: *stored_hg_cs_id,
                        bcs_id: entry.bcs_id,
                    },
                    entry.clone(),
                )
                .into());
            }
            match by_hg.get(&entry.hg_cs_id) {
                Some((stored_bubble_id, _)) if *stored_bubble_id != bubble_id => {
                    Err(EphemeralBlobstoreError::HgChangesetInAnotherBubble(
                        entry.hg_cs_id,
                        *stored_bubble_id,
                    )
                    .into())
                }
                Some((_, stored_bcs_id)) if *stored_bcs_id == entry.bcs_id => Ok(()),
                Some((_, stored_bcs_id)) => Err(BonsaiHgMappingError::ConflictingEntries(
                    BonsaiHgMappingEntry {
                        hg_cs_id: entry.hg_cs_id,
                        bcs_id: *stored_bcs_id,
                    },
                    entry.clone(),
                )
                .into()),
                None => Err(BonsaiHgMappingError::RaceConditionWithDelete(entry.clone()).into()),
            }
        })
    }

    pub(crate) async fn get(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        ids: BonsaiOrHgChangesetIds,
    ) -> Result<Vec<BonsaiHgMappingEntry>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let found = self
            .select(ctx, &self.connections.read_connection, bubble_id, &ids)
            .await?;
        let leftovers = leftover_ids(ids, &found);
        if leftovers.is_empty() {
            return Ok(found);
        }

        STATS::get_master_fallbacks.add_value(1);
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        let from_master = self
            .select(
                ctx,
                &self.connections.read_master_connection,
                bubble_id,
                &leftovers,
            )
            .await?;
        Ok(found.into_iter().chain(from_master).collect())
    }

    async fn select(
        &self,
        ctx: &CoreContext,
        connection: &Connection,
        bubble_id: BubbleId,
        ids: &BonsaiOrHgChangesetIds,
    ) -> Result<Vec<BonsaiHgMappingEntry>> {
        let rows = match ids {
            BonsaiOrHgChangesetIds::Bonsai(bcs_ids) => {
                SelectBubbleHgMappingByBonsai::query(
                    connection,
                    ctx.sql_query_telemetry(),
                    &self.repo_id,
                    &bubble_id,
                    bcs_ids.as_slice(),
                )
                .await?
            }
            BonsaiOrHgChangesetIds::Hg(hg_cs_ids) => {
                SelectBubbleHgMappingByHg::query(
                    connection,
                    ctx.sql_query_telemetry(),
                    &self.repo_id,
                    &bubble_id,
                    hg_cs_ids.as_slice(),
                )
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|(hg_cs_id, bcs_id)| BonsaiHgMappingEntry { hg_cs_id, bcs_id })
            .collect())
    }

    pub(crate) async fn get_hg_in_range(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        low: HgChangesetId,
        high: HgChangesetId,
        limit: usize,
    ) -> Result<Vec<HgChangesetId>> {
        if low > high {
            return Ok(Vec::new());
        }

        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let fetched = self
            .select_range(
                ctx,
                &self.connections.read_connection,
                bubble_id,
                low,
                high,
                limit,
            )
            .await?;
        if !fetched.is_empty() {
            return Ok(fetched);
        }

        STATS::get_master_fallbacks.add_value(1);
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        self.select_range(
            ctx,
            &self.connections.read_master_connection,
            bubble_id,
            low,
            high,
            limit,
        )
        .await
    }

    async fn select_range(
        &self,
        ctx: &CoreContext,
        connection: &Connection,
        bubble_id: BubbleId,
        low: HgChangesetId,
        high: HgChangesetId,
        limit: usize,
    ) -> Result<Vec<HgChangesetId>> {
        let rows = SelectBubbleHgChangesetsByRange::query(
            connection,
            ctx.sql_query_telemetry(),
            &self.repo_id,
            &bubble_id,
            &low.as_bytes(),
            &high.as_bytes(),
            &limit,
        )
        .await?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }
}

pub(crate) fn leftover_ids(
    ids: BonsaiOrHgChangesetIds,
    found: &[BonsaiHgMappingEntry],
) -> BonsaiOrHgChangesetIds {
    match ids {
        BonsaiOrHgChangesetIds::Bonsai(bcs_ids) => {
            let found = found
                .iter()
                .map(|entry| entry.bcs_id)
                .collect::<HashSet<_>>();
            BonsaiOrHgChangesetIds::Bonsai(
                bcs_ids
                    .into_iter()
                    .filter(|bcs_id| !found.contains(bcs_id))
                    .collect(),
            )
        }
        BonsaiOrHgChangesetIds::Hg(hg_cs_ids) => {
            let found = found
                .iter()
                .map(|entry| entry.hg_cs_id)
                .collect::<HashSet<_>>();
            BonsaiOrHgChangesetIds::Hg(
                hg_cs_ids
                    .into_iter()
                    .filter(|hg_cs_id| !found.contains(hg_cs_id))
                    .collect(),
            )
        }
    }
}

#[cfg(test)]
mod test {
    use std::slice;
    use std::sync::Arc;
    use std::time::Duration;

    use blobstore::BlobstoreEnumerableWithUnlink;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use mercurial_types_mocks::nodehash as hg;
    use metaconfig_types::BubbleDeletionMode;
    use mononoke_macros::mononoke;
    use mononoke_types_mocks::changesetid as bonsai;
    use mononoke_types_mocks::repo::REPO_ZERO;
    use sql_construct::SqlConstruct;

    use super::*;
    use crate::builder::RepoEphemeralStoreBuilder;
    use crate::store::RepoEphemeralStore;

    fn bootstrap(
        fb: FacebookInit,
    ) -> Result<(CoreContext, RepoEphemeralStore, SqlBubbleHgMapping)> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = Arc::new(Memblob::default()) as Arc<dyn BlobstoreEnumerableWithUnlink>;
        let eph = RepoEphemeralStoreBuilder::with_sqlite_in_memory()?.build(
            REPO_ZERO,
            blobstore,
            Duration::from_hours(24),
            Duration::from_hours(6),
            BubbleDeletionMode::MarkAndDelete,
        );
        let mapping = SqlBubbleHgMapping::new(eph.inner()?.connections.clone(), REPO_ZERO);
        Ok((ctx, eph, mapping))
    }

    fn entry(hg_cs_id: HgChangesetId, bcs_id: ChangesetId) -> BonsaiHgMappingEntry {
        BonsaiHgMappingEntry { hg_cs_id, bcs_id }
    }

    #[mononoke::fbinit_test]
    async fn add_then_get_by_hg_and_bonsai_test(fb: FacebookInit) -> Result<()> {
        let (ctx, eph, mapping) = bootstrap(fb)?;
        let a = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let b = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let ones = entry(hg::ONES_CSID, bonsai::ONES_CSID);

        assert_eq!(mapping.add(&ctx, a, slice::from_ref(&ones)).await?, 1);

        assert_eq!(
            mapping.get(&ctx, a, hg::ONES_CSID.into()).await?,
            vec![ones.clone()]
        );
        assert_eq!(
            mapping.get(&ctx, a, bonsai::ONES_CSID.into()).await?,
            vec![ones]
        );
        assert!(
            mapping.get(&ctx, b, hg::ONES_CSID.into()).await?.is_empty(),
            "another bubble must not see the row"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_is_idempotent_test(fb: FacebookInit) -> Result<()> {
        let (ctx, eph, mapping) = bootstrap(fb)?;
        let a = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let ones = entry(hg::ONES_CSID, bonsai::ONES_CSID);

        assert_eq!(mapping.add(&ctx, a, slice::from_ref(&ones)).await?, 1);
        assert_eq!(mapping.add(&ctx, a, slice::from_ref(&ones)).await?, 0);
        assert_eq!(
            mapping.get(&ctx, a, hg::ONES_CSID.into()).await?,
            vec![ones]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_batch_with_existing_row_test(fb: FacebookInit) -> Result<()> {
        let (ctx, eph, mapping) = bootstrap(fb)?;
        let a = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let ones = entry(hg::ONES_CSID, bonsai::ONES_CSID);
        let twos = entry(hg::TWOS_CSID, bonsai::TWOS_CSID);
        mapping.add(&ctx, a, slice::from_ref(&ones)).await?;

        assert_eq!(
            mapping.add(&ctx, a, &[ones.clone(), twos.clone()]).await?,
            1,
            "only the new row is inserted, the existing one is verified"
        );
        let mut stored = mapping
            .get(&ctx, a, vec![hg::ONES_CSID, hg::TWOS_CSID].into())
            .await?;
        stored.sort_by_key(|entry| entry.hg_cs_id);
        assert_eq!(stored, vec![ones, twos]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_conflicting_bonsai_test(fb: FacebookInit) -> Result<()> {
        let (ctx, eph, mapping) = bootstrap(fb)?;
        let a = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let stored = entry(hg::ONES_CSID, bonsai::ONES_CSID);
        let attempted = entry(hg::ONES_CSID, bonsai::TWOS_CSID);
        mapping.add(&ctx, a, slice::from_ref(&stored)).await?;

        let err = mapping
            .add(&ctx, a, slice::from_ref(&attempted))
            .await
            .expect_err("a different bonsai for the same hg must fail");

        assert_eq!(
            err.downcast_ref::<BonsaiHgMappingError>(),
            Some(&BonsaiHgMappingError::ConflictingEntries(stored, attempted))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_same_hg_other_bubble_test(fb: FacebookInit) -> Result<()> {
        let (ctx, eph, mapping) = bootstrap(fb)?;
        let a = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let b = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let ones = entry(hg::ONES_CSID, bonsai::ONES_CSID);
        mapping.add(&ctx, a, slice::from_ref(&ones)).await?;

        let err = mapping
            .add(&ctx, b, &[ones])
            .await
            .expect_err("the same hg in another bubble must fail");

        match err.downcast_ref::<EphemeralBlobstoreError>() {
            Some(EphemeralBlobstoreError::HgChangesetInAnotherBubble(hg_cs_id, bubble_id)) => {
                assert_eq!(*hg_cs_id, hg::ONES_CSID);
                assert_eq!(*bubble_id, a);
            }
            other => panic!("unexpected error: {other:?} ({err:#})"),
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn range_returns_only_rows_of_bubble_test(fb: FacebookInit) -> Result<()> {
        let (ctx, eph, mapping) = bootstrap(fb)?;
        let a = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        let b = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        mapping
            .add(
                &ctx,
                a,
                &[
                    entry(hg::ONES_CSID, bonsai::ONES_CSID),
                    entry(hg::TWOS_CSID, bonsai::TWOS_CSID),
                ],
            )
            .await?;
        mapping
            .add(&ctx, b, &[entry(hg::THREES_CSID, bonsai::THREES_CSID)])
            .await?;

        let high = HgChangesetId::from_bytes(&[0xff; 20])?;
        assert_eq!(
            mapping
                .get_hg_in_range(&ctx, a, hg::NULL_CSID, high, 10)
                .await?,
            vec![hg::ONES_CSID, hg::TWOS_CSID],
            "bubble b's row is not in bubble a's range"
        );
        Ok(())
    }
}
