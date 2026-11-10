/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Resetting the retired types' metadata: enablement markers, root mappings
//! and the shared-cache entries of those mappings.
//!
//! Only rows of the selected repositories and types, at the versions the
//! policy approves, are deleted, through the primary.  Rows of other
//! versions are reported and left alone.  Every deleted mapping is journalled
//! so its shared-cache entry can be evicted again after the blobs are gone.
//! In-process caches of running services cannot be reached from here.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use clap::Args;
use commit_derived_data_mapping::CacheEviction;
use commit_derived_data_mapping::CommitDerivedDataMapping;
use context::CoreContext;
use enabled_derived_data_types::EnabledDerivedDataTypes;
use itertools::Itertools;
use mononoke_app::MononokeApp;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use serde::Deserialize;
use serde::Serialize;

use crate::policy::RetiredBlobType;
use crate::policy::Selection;
use crate::state::StateDir;

#[derive(Args, Clone, Debug)]
pub struct MetadataOptions {
    /// Mapping rows listed per query, and deleted per batch
    #[clap(long, default_value_t = NonZeroU32::new(1000).expect("1000 is non-zero"))]
    pub mapping_page_size: NonZeroU32,
    /// Leave shared-cache entries in place.  Reports then show caches as not
    /// invalidated
    #[clap(long)]
    pub skip_cache_eviction: bool,
}

/// How the shared-cache entries of a batch of deleted mappings were handled.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    Evicted,
    /// The tool ran without a shared cache, so nothing could be evicted.
    NoSharedCache,
    Skipped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum MappingEvent {
    /// Recorded before the mappings are deleted, so their cache entries are
    /// evicted again even if this run stops before recording the outcome.
    Deleting {
        version: i32,
        cs_ids: Vec<ChangesetId>,
    },
    /// The outcome of the preceding `Deleting` batch.
    Deleted { rows: u64, cache: CacheState },
}

/// The outcome of resetting one repository's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataOutcome {
    pub rows_deleted: u64,
    pub cache_entries_evicted: u64,
    /// Batches whose shared-cache entries were not evicted.
    pub cache_not_invalidated: u64,
    /// Rows left in place because the policy does not approve their version,
    /// by type and version.
    pub unapproved_rows: BTreeMap<String, u64>,
}

impl MetadataOutcome {
    fn add(&mut self, other: MetadataOutcome) {
        self.rows_deleted += other.rows_deleted;
        self.cache_entries_evicted += other.cache_entries_evicted;
        self.cache_not_invalidated += other.cache_not_invalidated;
        for (rows, count) in other.unapproved_rows {
            *self.unapproved_rows.entry(rows).or_default() += count;
        }
    }

    fn record_batch(&mut self, rows: u64, cache: CacheState, entries: usize) {
        self.rows_deleted += rows;
        match cache {
            CacheState::Evicted => self.cache_entries_evicted += entries as u64,
            CacheState::NoSharedCache | CacheState::Skipped => self.cache_not_invalidated += 1,
        }
    }
}

/// The record of a repository's final metadata pass: how the shared-cache
/// entries of every journalled batch were handled after the blobs were
/// gone.  A metadata reset removes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalPass {
    pub batches: BTreeMap<CacheState, u64>,
    pub entries_evicted: u64,
}

/// The metadata stores of one repository.
pub struct RepoMetadata {
    pub mapping: Arc<CommitDerivedDataMapping>,
    pub enabled: Arc<dyn EnabledDerivedDataTypes>,
}

impl RepoMetadata {
    pub async fn open(app: &MononokeApp, repo_name: &str) -> Result<Self> {
        let repo_config = Arc::new(
            app.repo_config_by_name(repo_name)
                .with_context(|| format!("loading the config of {repo_name}"))?,
        );
        let factory = app.repo_factory();
        Ok(Self {
            mapping: factory.commit_derived_data_mapping(&repo_config).await?,
            enabled: factory.enabled_derived_data_types(&repo_config).await?,
        })
    }
}

fn final_pass_path(repo_id: RepositoryId) -> PathBuf {
    PathBuf::from("metadata")
        .join(repo_id.id().to_string())
        .join("final.json")
}

fn journal_path(repo_id: RepositoryId, blob_type: RetiredBlobType, shard_id: usize) -> PathBuf {
    PathBuf::from("metadata")
        .join(repo_id.id().to_string())
        .join(blob_type.to_string())
        .join(format!("shard{shard_id}.jsonl"))
}

/// Resetting one repository's metadata.
pub struct MetadataContext<'a> {
    pub ctx: &'a CoreContext,
    pub state: &'a StateDir,
    pub md: &'a RepoMetadata,
    pub repo_id: RepositoryId,
    pub selection: &'a Selection,
    pub options: &'a MetadataOptions,
}

impl MetadataContext<'_> {
    async fn evict(
        &self,
        blob_type: RetiredBlobType,
        version: i32,
        shard_id: usize,
        cs_ids: &[ChangesetId],
    ) -> Result<CacheState> {
        if self.options.skip_cache_eviction {
            return Ok(CacheState::Skipped);
        }
        let eviction = self
            .md
            .mapping
            .evict_shared_cache(
                self.repo_id,
                blob_type.derivable_type(),
                version,
                shard_id,
                cs_ids,
            )
            .await
            .with_context(|| {
                format!(
                    "evicting {} {blob_type} v{version} mappings of {}",
                    cs_ids.len(),
                    self.repo_id
                )
            })?;
        Ok(match eviction {
            CacheEviction::Evicted => CacheState::Evicted,
            CacheEviction::NoSharedCache => CacheState::NoSharedCache,
        })
    }

    /// Disables the selected types for the repository and deletes their
    /// approved mapping rows on every connection, evicting each deleted
    /// mapping's shared-cache entry.  Removes any final pass record.
    pub async fn reset(&self) -> Result<MetadataOutcome> {
        self.state
            .remove_file(&final_pass_path(self.repo_id))
            .await?;
        let mut outcome = MetadataOutcome::default();
        for blob_type in self.selection.types() {
            let dt = blob_type.derivable_type();
            self.md
                .enabled
                .mark_disabled(self.ctx, self.repo_id, dt)
                .await
                .with_context(|| format!("disabling {blob_type} for {}", self.repo_id))?;
            for shard_id in 0..self.md.mapping.sql.connection_count() {
                outcome.add(self.reset_shard(blob_type, shard_id).await?);
            }
        }
        Ok(outcome)
    }

    async fn reset_shard(
        &self,
        blob_type: RetiredBlobType,
        shard_id: usize,
    ) -> Result<MetadataOutcome> {
        let dt = blob_type.derivable_type();
        let page_size = self.options.mapping_page_size;
        let mut journal = self
            .state
            .journal(&journal_path(self.repo_id, blob_type, shard_id));
        let mut outcome = MetadataOutcome::default();
        let mut after = None;
        loop {
            let rows = self
                .md
                .mapping
                .sql
                .list_type_mappings(self.ctx, self.repo_id, dt, after, page_size, shard_id)
                .await
                .with_context(|| {
                    format!(
                        "listing {blob_type} mappings of {} on connection {shard_id}",
                        self.repo_id
                    )
                })?;
            let Some(last) = rows.last().copied() else {
                return Ok(outcome);
            };
            let full_page = rows.len() == page_size.get() as usize;
            let by_version = rows
                .into_iter()
                .map(|(cs_id, version)| (version, cs_id))
                .into_group_map()
                .into_iter()
                .sorted_by_key(|(version, _)| *version);
            for (version, cs_ids) in by_version {
                if !blob_type.mapping_versions().contains(&version) {
                    *outcome
                        .unapproved_rows
                        .entry(format!("{blob_type} v{version}"))
                        .or_default() += cs_ids.len() as u64;
                    continue;
                }
                journal
                    .append(&MappingEvent::Deleting {
                        version,
                        cs_ids: cs_ids.clone(),
                    })
                    .await?;
                let rows = self
                    .md
                    .mapping
                    .sql
                    .delete_mappings(self.ctx, self.repo_id, dt, version, &cs_ids, shard_id)
                    .await
                    .with_context(|| {
                        format!(
                            "deleting {} {blob_type} v{version} mappings of {}",
                            cs_ids.len(),
                            self.repo_id
                        )
                    })?;
                let cache = self.evict(blob_type, version, shard_id, &cs_ids).await?;
                journal
                    .append(&MappingEvent::Deleted { rows, cache })
                    .await?;
                outcome.record_batch(rows, cache, cs_ids.len());
            }
            if !full_page {
                return Ok(outcome);
            }
            after = Some(last);
        }
    }

    /// Evicts again the shared-cache entries of every mapping this run has
    /// deleted, including batches whose outcome was never recorded, and
    /// records the result as the repository's final pass.
    pub async fn evict_journalled(&self) -> Result<FinalPass> {
        let mut pass = FinalPass::default();
        for blob_type in self.selection.types() {
            for shard_id in 0..self.md.mapping.sql.connection_count() {
                let events: Vec<MappingEvent> = self
                    .state
                    .read_journal(&journal_path(self.repo_id, blob_type, shard_id))
                    .await?;
                for event in events {
                    let MappingEvent::Deleting { version, cs_ids } = event else {
                        continue;
                    };
                    let cache = self.evict(blob_type, version, shard_id, &cs_ids).await?;
                    *pass.batches.entry(cache).or_default() += 1;
                    if cache == CacheState::Evicted {
                        pass.entries_evicted += cs_ids.len() as u64;
                    }
                }
            }
        }
        self.state
            .write_json(&final_pass_path(self.repo_id), &pass)
            .await?;
        Ok(pass)
    }
}

#[cfg(test)]
mod tests {
    use caching_ext::CacheHandlerFactory;
    use commit_derived_data_mapping::SqlCommitDerivedDataMapping;
    use enabled_derived_data_types::Staleness;
    use enabled_derived_data_types::TestEnabledDerivedDataTypes;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use mononoke_types::DerivableType;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use mononoke_types_mocks::changesetid::THREES_CSID;
    use mononoke_types_mocks::changesetid::TWOS_CSID;
    use sql_construct::SqlConstruct;

    use super::*;
    use crate::scan::tests::test_plan;
    use crate::scan::tests::test_state;

    const VALUE: [u8; 32] = [7u8; 32];

    fn page(rows: u32) -> NonZeroU32 {
        NonZeroU32::new(rows).expect("test pages are non-zero")
    }

    fn cached_metadata() -> Result<(RepoMetadata, Arc<TestEnabledDerivedDataTypes>)> {
        let enabled = Arc::new(TestEnabledDerivedDataTypes::new());
        let md = RepoMetadata {
            mapping:
                Arc::new(
                    CommitDerivedDataMapping::new(
                        SqlCommitDerivedDataMapping::with_sqlite_in_memory()?,
                    )
                    .with_caching(CacheHandlerFactory::Mocked),
                ),
            enabled: enabled.clone(),
        };
        Ok((md, enabled))
    }

    fn context<'a>(
        ctx: &'a CoreContext,
        state: &'a StateDir,
        md: &'a RepoMetadata,
        repo_id: RepositoryId,
        selection: &'a Selection,
        options: &'a MetadataOptions,
    ) -> MetadataContext<'a> {
        MetadataContext {
            ctx,
            state,
            md,
            repo_id,
            selection,
            options,
        }
    }

    #[mononoke::fbinit_test]
    async fn reset_deletes_only_approved_rows_of_selected_types(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (md, enabled) = cached_metadata()?;
        let mapping = md.mapping.clone();
        let repo = RepositoryId::new(1);
        let other = RepositoryId::new(2);
        let rows = [
            (repo, ONES_CSID, DerivableType::HistoryManifests, 1),
            (repo, TWOS_CSID, DerivableType::HistoryManifests, 1),
            (repo, THREES_CSID, DerivableType::HistoryManifests, 1),
            (repo, ONES_CSID, DerivableType::FastlogV2, 1),
            (repo, ONES_CSID, DerivableType::BlameV3, 1),
            (repo, ONES_CSID, DerivableType::BlameV3, 2),
            // Not approved by the policy: left in place and reported.
            (repo, TWOS_CSID, DerivableType::BlameV3, 7),
            // Not selected, or another repository.
            (repo, ONES_CSID, DerivableType::Unodes, 1),
            (other, ONES_CSID, DerivableType::HistoryManifests, 1),
        ];
        for (repo_id, cs_id, dt, version) in rows {
            mapping
                .store_mapping(&ctx, repo_id, cs_id, dt, version, &VALUE, 0)
                .await?;
        }
        for dt in [DerivableType::HistoryManifests, DerivableType::Unodes] {
            enabled.mark_enabled(&ctx, repo, dt, None).await?;
        }

        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let selection = Selection::new(RetiredBlobType::all())?;
        let options = MetadataOptions {
            mapping_page_size: page(2),
            skip_cache_eviction: false,
        };
        let mctx = context(&ctx, &state, &md, repo, &selection, &options);
        let outcome = mctx.reset().await?;
        assert_eq!(outcome.rows_deleted, 6);
        assert_eq!(outcome.cache_entries_evicted, 6);
        assert_eq!(outcome.cache_not_invalidated, 0);
        assert_eq!(
            outcome.unapproved_rows,
            BTreeMap::from([("blame-v3 v7".to_string(), 1)])
        );

        assert_eq!(
            mapping
                .sql
                .list_type_mappings(&ctx, repo, DerivableType::BlameV3, None, page(10), 0)
                .await?,
            vec![(TWOS_CSID, 7)],
            "only the unapproved row is left"
        );
        assert_eq!(
            enabled
                .get_enabled_types(&ctx, repo, Staleness::MostRecent)
                .await?,
            vec![DerivableType::Unodes]
        );
        for (repo_id, dt) in [
            (repo, DerivableType::Unodes),
            (other, DerivableType::HistoryManifests),
        ] {
            assert_eq!(
                mapping
                    .sql
                    .list_type_mappings(&ctx, repo_id, dt, None, page(10), 0)
                    .await?
                    .len(),
                1,
                "{dt:?} of {repo_id} must be untouched"
            );
        }

        // Pages of two: history manifests in two batches, fastlog in one,
        // blame in one per approved version.
        let final_path = final_pass_path(repo);
        assert!(!state.exists(&final_path).await?);
        let pass = mctx.evict_journalled().await?;
        assert_eq!(pass.entries_evicted, 6);
        assert_eq!(pass.batches, BTreeMap::from([(CacheState::Evicted, 5)]));
        assert_eq!(state.read_json(&final_path).await?, Some(pass));
        mctx.reset().await?;
        assert!(
            !state.exists(&final_path).await?,
            "a reset invalidates the final pass"
        );

        let uncached = RepoMetadata {
            mapping: Arc::new(CommitDerivedDataMapping::new(
                SqlCommitDerivedDataMapping::with_sqlite_in_memory()?,
            )),
            enabled,
        };
        uncached
            .mapping
            .store_mapping(
                &ctx,
                other,
                ONES_CSID,
                DerivableType::FastlogV2,
                1,
                &VALUE,
                0,
            )
            .await?;
        let mctx = context(&ctx, &state, &uncached, other, &selection, &options);
        let outcome = mctx.reset().await?;
        assert_eq!(outcome.rows_deleted, 1);
        assert_eq!(
            outcome.cache_not_invalidated, 1,
            "without a shared cache the batch is reported as not invalidated"
        );
        assert_eq!(
            mctx.evict_journalled().await?.batches,
            BTreeMap::from([(CacheState::NoSharedCache, 1)])
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_batch_interrupted_before_its_outcome_is_evicted_by_the_final_pass(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (md, _) = cached_metadata()?;
        let repo = RepositoryId::new(1);
        let hm = DerivableType::HistoryManifests;
        for cs_id in [ONES_CSID, TWOS_CSID] {
            md.mapping
                .store_mapping(&ctx, repo, cs_id, hm, 1, &VALUE, 0)
                .await?;
        }
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let selection = Selection::new(RetiredBlobType::all())?;
        let options = MetadataOptions {
            mapping_page_size: page(10),
            skip_cache_eviction: false,
        };
        // A run journalled a batch, then failed before deleting it.
        state
            .journal(&journal_path(repo, RetiredBlobType::HistoryManifests, 0))
            .append(&MappingEvent::Deleting {
                version: 1,
                cs_ids: vec![ONES_CSID, TWOS_CSID],
            })
            .await?;

        let mctx = context(&ctx, &state, &md, repo, &selection, &options);
        let rescan = mctx.reset().await?;
        assert_eq!(rescan.rows_deleted, 2, "the rescan deletes the rows left");
        let pass = mctx.evict_journalled().await?;
        assert_eq!(
            pass.batches,
            BTreeMap::from([(CacheState::Evicted, 2)]),
            "the interrupted batch is evicted again with the rescan's"
        );
        assert!(
            md.mapping
                .sql
                .list_type_mappings(&ctx, repo, hm, None, page(10), 0)
                .await?
                .is_empty()
        );
        Ok(())
    }
}
