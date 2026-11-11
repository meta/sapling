/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Ephemeral Store

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use blobstore::BlobstoreEnumerableWithUnlink;
use chrono::Duration as ChronoDuration;
use context::CoreContext;
use context::PerfCounterType;
use derivative::Derivative;
use futures::Stream;
use futures::StreamExt;
use futures::stream;
use futures_lazy_shared::LazyShared;
use mercurial_types::HgChangesetId;
use metaconfig_types::BubbleDeletionMode;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use sql_ext::SqlConnections;
use sql_ext::mononoke_queries;
use sql_query_config::SqlQueryConfig;
use stats::prelude::*;

use crate::bubble::Bubble;
use crate::bubble::BubbleId;
use crate::bubble::ExpiryStatus;
use crate::error::EphemeralBlobstoreError;
use crate::unlink::UnlinkPolicy;
use crate::unlink::Unlinker;

mod hg_mapping;

pub(crate) use hg_mapping::SqlBubbleHgMapping;
pub(crate) use hg_mapping::leftover_ids;

define_stats! {
    prefix = "mononoke.ephemeral_blobstore.hg_mapping";
    hg_mapping_rows_deleted: timeseries(Sum),
}

/// Outcome of extending a bubble's TTL
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtendBubbleTtlOutcome {
    /// The bubble's TTL was extended to the new timestamp
    Extended(Timestamp),
    /// The bubble's TTL was not changed, current expiration timestamp
    NotChanged(Timestamp),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeleteBubbleOutcome {
    pub keys_deleted: usize,
    pub keys_already_deleted: usize,
}

/// Ephemeral Store.
#[derive(Derivative)]
#[derivative(Debug)]
struct RepoEphemeralStoreInner {
    /// The backing blobstore where blobs are stored, without any redaction
    /// or repo prefix wrappers.
    pub(crate) blobstore: Arc<dyn BlobstoreEnumerableWithUnlink>,

    #[derivative(Debug = "ignore")]
    /// Database used to manage the ephemeral store.
    pub(crate) connections: SqlConnections,

    /// Initial value of the lifespan for bubbles in this store, i.e. the
    /// amount of time they last from the call to create.
    pub(crate) initial_bubble_lifespan: ChronoDuration,

    /// Grace period after bubbles expire during which requests which have
    /// already opened a bubble can continue to access them.  The bubble
    /// contents will not be deleted until after the grace period.
    pub(crate) bubble_expiration_grace: ChronoDuration,

    /// The mode of deletion to be used when cleaning up expired bubbles.
    /// The value determines if the bubbles need to be simply marked as
    /// expired or actually deleted from the physical store.
    pub(crate) bubble_deletion_mode: BubbleDeletionMode,
}

/// Ephemeral Store
#[facet::facet]
#[derive(Debug, Clone)]
pub struct RepoEphemeralStore {
    /// Repo this belongs to
    repo_id: RepositoryId,
    inner: Option<Arc<RepoEphemeralStoreInner>>,
}

mononoke_queries! {
    write CreateBubble(
        created_at: Timestamp,
        expires_at: Timestamp,
        owner_identity: Option<&str>,
    ) {
        none,
        "INSERT INTO ephemeral_bubbles (created_at, expires_at, owner_identity)
         VALUES ({created_at}, {expires_at}, {owner_identity})"
    }

    write AddBubbleLabels(
        values: (
            bubble_id: BubbleId,
            label: str,
    )) {
        insert_or_ignore,
        "{insert_or_ignore} INTO ephemeral_bubble_labels (bubble_id, label)
        VALUES {values}"
    }

    write DeleteBubbleLabels(
        id: BubbleId,
        >list labels: &str
    ) {
        none,
        "DELETE FROM ephemeral_bubble_labels WHERE
        bubble_id = {id} AND label IN {labels}"
    }

    write DeleteBubbleLabelsById(
        id: BubbleId,
    ) {
        none,
        "DELETE FROM ephemeral_bubble_labels WHERE
        bubble_id = {id}"
    }

    read SelectBubbleById(
        id: BubbleId,
    ) -> (Timestamp, ExpiryStatus, Option<String>) {
        "SELECT expires_at, expired, owner_identity FROM ephemeral_bubbles
         WHERE id = {id}"
    }

    read SelectBubbleLabelsById(
        id: BubbleId,
    ) -> (String, ) {
        "SELECT label FROM ephemeral_bubble_labels
        WHERE bubble_id = {id}"
    }

    read SelectBubbleFromChangeset(
        repo_id: RepositoryId,
        cs_id: ChangesetId,
    ) -> (BubbleId,) {
        "SELECT bubble_id
        FROM ephemeral_bubble_changeset_mapping
        WHERE repo_id = {repo_id} AND cs_id = {cs_id}"
    }

    cacheable read SelectBubbleByIdMaybeStale(
         id: BubbleId,
    ) -> (Timestamp, ExpiryStatus, Option<String>) {
         "SELECT expires_at, expired, owner_identity FROM ephemeral_bubbles
         WHERE id={id}"
    }

    read SelectChangesetFromBubble(
        id: BubbleId,
    ) -> (ChangesetId,) {
        "SELECT cs_id
        FROM ephemeral_bubble_changeset_mapping
        WHERE bubble_id = {id}"
    }

    read SelectBubblesWithExpiry(
        expires_at: Timestamp,
        limit: u32,
    ) -> (BubbleId,) {
        "SELECT B.id
        FROM ephemeral_bubbles B
        WHERE B.expires_at < {expires_at}
        AND NOT EXISTS (SELECT id from ephemeral_bubble_labels L WHERE B.id = L.bubble_id LIMIT 1)
        LIMIT {limit}"
    }

    read SelectBubblesWithExpiryAndStatus(
        expires_at: Timestamp,
        limit: u32,
        expiry_status: ExpiryStatus,
    ) -> (BubbleId,) {
        "SELECT B.id
        FROM ephemeral_bubbles B
        WHERE B.expires_at < {expires_at} AND B.expired = {expiry_status}
        AND NOT EXISTS (SELECT id FROM ephemeral_bubble_labels L WHERE B.id = L.bubble_id LIMIT 1)
        LIMIT {limit}"
    }

    write UpdateExpired(
        expired: ExpiryStatus,
        id: BubbleId
    ) {
        none,
        "UPDATE ephemeral_bubbles
        SET expired={expired}
        WHERE id={id}"
    }

    write UpdateBubbleExpiration(
        expires_at: Timestamp,
        id: BubbleId
    ) {
        none,
        "UPDATE ephemeral_bubbles
        SET expires_at={expires_at}
        WHERE id={id} AND expired = 0"
    }

    write DeleteBubble(
        id: BubbleId,
    ) {
        none,
        "DELETE
        FROM ephemeral_bubbles
        WHERE id={id} AND expired"
    }

    write DeleteBubbleChangesetMapping(
        id: BubbleId,
    ) {
        none,
        "DELETE
        FROM ephemeral_bubble_changeset_mapping
        WHERE bubble_id IN (SELECT id FROM ephemeral_bubbles WHERE id = {id} AND expired)"
    }

    write DeleteBubbleChangesetParents(
        id: BubbleId,
    ) {
        none,
        "DELETE
        FROM ephemeral_bubble_changeset_parents
        WHERE bubble_id IN (SELECT id FROM ephemeral_bubbles WHERE id = {id} AND expired)"
    }

    write DeleteBubbleHgMapping(
        id: BubbleId,
    ) {
        none,
        "DELETE
        FROM ephemeral_bubble_hg_mapping
        WHERE bubble_id IN (SELECT id FROM ephemeral_bubbles WHERE id = {id} AND expired)"
    }

    write DeleteExpiredBubbleLabels(
        id: BubbleId,
    ) {
        none,
        "DELETE
        FROM ephemeral_bubble_labels
        WHERE bubble_id IN (SELECT id FROM ephemeral_bubbles WHERE id = {id} AND expired)"
    }
}

// Approximating a big duration to max chrono duration (~10^11 years) is good enough
fn to_chrono(duration: Duration) -> ChronoDuration {
    ChronoDuration::from_std(duration).unwrap_or(ChronoDuration::MAX)
}

impl RepoEphemeralStoreInner {
    async fn create_bubble(
        &self,
        ctx: &CoreContext,
        custom_duration: Option<Duration>,
        labels: Vec<String>,
    ) -> Result<Bubble> {
        let created_at = DateTime::now();
        let duration = match custom_duration {
            None => self.initial_bubble_lifespan,
            Some(duration) => to_chrono(duration),
        };
        let expires_at = created_at + duration;

        let txn = self
            .connections
            .write_connection
            .start_transaction(ctx.sql_query_telemetry())
            .await?;
        let (txn, res) = CreateBubble::query_with_transaction(
            txn,
            &Timestamp::from(created_at),
            &Timestamp::from(expires_at),
            &None,
        )
        .await?;

        match res.last_insert_id() {
            Some(id) if res.affected_rows() == 1 => {
                let bubble_id = BubbleId::new(
                    std::num::NonZeroU64::new(id)
                        .ok_or(EphemeralBlobstoreError::CreateBubbleFailed)?,
                );
                if !labels.is_empty() {
                    let bubble_labels = labels
                        .iter()
                        .map(|label| (&bubble_id, label as &str))
                        .collect::<Vec<_>>();
                    let (txn, _res) =
                        AddBubbleLabels::query_with_transaction(txn, bubble_labels.as_slice())
                            .await?;
                    txn.commit().await?;
                } else {
                    txn.commit().await?;
                };
                ctx.perf_counters()
                    .increment_counter(PerfCounterType::SqlWrites);

                Ok(Bubble::new(
                    bubble_id,
                    expires_at + self.bubble_expiration_grace,
                    self.blobstore.clone(),
                    self.connections.clone(),
                    ExpiryStatus::Active,
                    LazyShared::new_ready(Ok(labels)),
                ))
            }
            _ => Err(EphemeralBlobstoreError::CreateBubbleFailed.into()),
        }
    }

    /// Add labels to an existing bubble
    async fn add_bubble_labels(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        labels: Vec<String>,
    ) -> Result<()> {
        // Open the bubble to validate if the bubble exists and has not expired.
        self.open_bubble(ctx, bubble_id).await?;
        let bubble_labels = labels
            .iter()
            .map(|label| (&bubble_id, label.as_str()))
            .collect::<Vec<_>>();
        // The bubble exists, add labels to it.
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        AddBubbleLabels::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            bubble_labels.as_slice(),
        )
        .await?;
        Ok(())
    }

    /// Remove labels associated with an existing bubble
    async fn remove_bubble_labels(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        labels: Vec<String>,
    ) -> Result<()> {
        // Open the bubble to validate if the bubble exists and has not expired.
        let bubble = self.open_bubble(ctx, bubble_id).await?;
        let labels = labels
            .iter()
            .map(|label| label.as_str())
            .collect::<Vec<_>>();
        // The bubble exists, remove labels from it.
        if labels.is_empty() {
            // No input labels were provided. In this case remove_labels should remove all
            // labels associated with the bubble.
            ctx.perf_counters()
                .increment_counter(PerfCounterType::SqlWrites);
            DeleteBubbleLabelsById::query(
                &self.connections.write_connection,
                ctx.sql_query_telemetry(),
                &bubble.bubble_id(),
            )
            .await?;
        } else {
            // Specific labels were provided as input. Only remove those labels.
            ctx.perf_counters()
                .increment_counter(PerfCounterType::SqlWrites);
            DeleteBubbleLabels::query(
                &self.connections.write_connection,
                ctx.sql_query_telemetry(),
                &bubble.bubble_id(),
                labels.as_slice(),
            )
            .await?;
        }
        Ok(())
    }

    /// Given a changeset ID, fetch the corresponding Bubble ID.
    async fn bubble_from_changeset(
        &self,
        ctx: &CoreContext,
        repo_id: &RepositoryId,
        cs_id: &ChangesetId,
    ) -> Result<Option<BubbleId>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let rows = SelectBubbleFromChangeset::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            repo_id,
            cs_id,
        )
        .await?;
        Ok(rows.into_iter().next().map(|b| b.0))
    }

    async fn bubble_from_hg_changeset(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        hg_cs_id: HgChangesetId,
    ) -> Result<Option<BubbleId>> {
        let expires_after = Timestamp::from(DateTime::now() - self.bubble_expiration_grace);
        SqlBubbleHgMapping::new(self.connections.clone(), repo_id)
            .live_bubble_for_hg(ctx, hg_cs_id, expires_after)
            .await
    }

    /// Given a Bubble ID, fetch labels corresponding to that bubble.
    async fn labels_from_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: &BubbleId,
    ) -> Result<Vec<String>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let rows = SelectBubbleLabelsById::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            bubble_id,
        )
        .await?;
        Ok(rows.into_iter().map(|l| l.0).collect::<Vec<_>>())
    }

    async fn changesets_from_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: &BubbleId,
    ) -> Result<Vec<ChangesetId>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let rows = SelectChangesetFromBubble::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            bubble_id,
        )
        .await?;
        Ok(rows.into_iter().map(|b| b.0).collect::<Vec<_>>())
    }

    /// Gets the vector of bubbles that are past their expiry period
    /// by at least a duration of expiry_offset + bubble_expiration_grace
    async fn get_expired_bubbles(
        &self,
        ctx: &CoreContext,
        expiry_offset: Duration,
        max_bubbles: u32,
    ) -> Result<Vec<BubbleId>> {
        let expiry_cutoff = DateTime::now() - to_chrono(expiry_offset);
        let rows = match self.bubble_deletion_mode {
            // If deletion mode is MarkOnly, we want to fetch only those
            // bubbles that are past expiry period but NOT marked as
            // expired yet (i.e. are active)
            BubbleDeletionMode::MarkOnly => {
                ctx.perf_counters()
                    .increment_counter(PerfCounterType::SqlWrites);
                SelectBubblesWithExpiryAndStatus::query(
                    &self.connections.write_connection,
                    ctx.sql_query_telemetry(),
                    &Timestamp::from(expiry_cutoff - self.bubble_expiration_grace),
                    &max_bubbles,
                    &ExpiryStatus::Active,
                )
                .await?
            }
            // If hard delete is required, we want to fetch bubbles regardless
            // of their expiry status as long as they are past expiry date.
            _ => {
                ctx.perf_counters()
                    .increment_counter(PerfCounterType::SqlWrites);
                SelectBubblesWithExpiry::query(
                    &self.connections.write_connection,
                    ctx.sql_query_telemetry(),
                    &Timestamp::from(expiry_cutoff - self.bubble_expiration_grace),
                    &max_bubbles,
                )
                .await?
            }
        };
        Ok(rows.into_iter().map(|b| b.0).collect::<Vec<_>>())
    }

    async fn keys_in_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        start_from: Option<String>,
        max: u32,
    ) -> Result<Vec<String>> {
        let bubble = self.open_bubble_raw(ctx, bubble_id, false).await?;
        bubble.keys_in_bubble(ctx, start_from, max).await
    }

    /// Method responsible for deleting the bubble and all the data contained within.
    /// Returns the number of blobs deleted from the bubble and the number of blobs
    /// that were already deleted.
    async fn delete_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        unlinker: &Unlinker,
    ) -> Result<DeleteBubbleOutcome> {
        // Step 0: Validate if bubble deletion is enabled.
        if let BubbleDeletionMode::Disabled = self.bubble_deletion_mode {
            return Err(EphemeralBlobstoreError::DeleteBubbleDisabled.into());
        }
        // Step 1: Mark the bubble as expired in the backing SQL Store.
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        UpdateExpired::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &ExpiryStatus::Expired,
            &bubble_id,
        )
        .await?;
        // If only marking is required, exit now.
        if let BubbleDeletionMode::MarkOnly = self.bubble_deletion_mode {
            return Ok(DeleteBubbleOutcome::default()); // Since 0 blob items were unlinked/removed.
        }
        // Step 2: Delete the blob content within the expired bubble.
        let bubble = self.open_bubble_raw(ctx, bubble_id, false).await?;
        let outcome = bubble.delete_blobs_in_bubble(ctx, unlinker).await?;

        // Step 3: Delete the metadata associated with the bubble from
        // the backing SQL store.
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        DeleteBubbleChangesetMapping::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?;
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        DeleteBubbleChangesetParents::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?;
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        let hg_mapping_rows = DeleteBubbleHgMapping::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?
        .affected_rows();
        STATS::hg_mapping_rows_deleted.add_value(hg_mapping_rows as i64);
        // When invoked through processes, this will always be a no-op. This is useful for
        // manually deleting a bubble regardless of its expiry status.
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        DeleteExpiredBubbleLabels::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?;

        // Step 4: Delete the bubble itself from the backing SQL store.
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        DeleteBubble::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?;
        Ok(outcome)
    }

    async fn open_bubble_raw(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        fail_on_expired: bool,
    ) -> Result<Bubble> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let mut bubble_rows = SelectBubbleById::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?;
        let labels_future = {
            let ctx = ctx.clone();
            let connection = self.connections.read_connection.clone();
            let bubble_id = bubble_id.clone();
            async move {
                ctx.perf_counters()
                    .increment_counter(PerfCounterType::SqlReadsReplica);
                let label_rows = SelectBubbleLabelsById::query(
                    &connection,
                    ctx.sql_query_telemetry(),
                    &bubble_id,
                )
                .await
                .map_err(|_| EphemeralBlobstoreError::FetchBubbleLabelsFailed(bubble_id))?;
                let labels = label_rows.into_iter().map(|l| l.0).collect::<Vec<_>>();
                Ok(labels)
            }
        };
        let labels_lazy = LazyShared::new_future(labels_future);

        if bubble_rows.is_empty() {
            // Perhaps the bubble hasn't showed up yet due to replication lag.
            // Let's retry on master just in case.
            ctx.perf_counters()
                .increment_counter(PerfCounterType::SqlReadsMaster);
            bubble_rows = SelectBubbleById::query(
                &self.connections.read_master_connection,
                ctx.sql_query_telemetry(),
                &bubble_id,
            )
            .await?;
            if bubble_rows.is_empty() {
                // S391328: Still no bubble.  Let's wait a short while and try
                // again.
                tokio::time::sleep(Duration::from_millis(500)).await;
                ctx.perf_counters()
                    .increment_counter(PerfCounterType::SqlReadsMaster);
                bubble_rows = SelectBubbleById::query(
                    &self.connections.read_master_connection,
                    ctx.sql_query_telemetry(),
                    &bubble_id,
                )
                .await?;
                if bubble_rows.is_empty() {
                    return Err(EphemeralBlobstoreError::NoSuchBubble(bubble_id).into());
                }
                let mut scuba = ctx.scuba().clone();
                ctx.perf_counters().insert_perf_counters(&mut scuba);
                scuba.log_with_msg("Bubble appeared after short wait", None);
            }
        }
        // TODO(mbthomas): check owner_identity
        let (expires_at, expiry_status, ref _owner_identity) = bubble_rows[0];
        let db_expires_at: DateTime = expires_at.into();
        let expires_at = db_expires_at + self.bubble_expiration_grace;
        let now = DateTime::now();

        // Check if the bubble has expired based on both the expiry_status and the actual expiration time
        if fail_on_expired {
            // If the bubble is marked as expired, it's definitely expired
            if expiry_status == ExpiryStatus::Expired {
                return Err(EphemeralBlobstoreError::NoSuchBubble(bubble_id).into());
            }

            // Even if not marked as expired, check if it has actually expired based on time
            // Note: expires_at from the database is the original expiration time without grace period
            // Only allow access if the bubble has labels (which would keep it alive past expiry)
            if now > expires_at {
                // Check if the bubble has labels that would keep it alive
                let labels = labels_lazy
                    .get_or_init(|| async { Ok(Vec::new()) })
                    .await
                    .map_err(|_| EphemeralBlobstoreError::NoSuchBubble(bubble_id))?;
                if labels.is_empty() {
                    return Err(EphemeralBlobstoreError::NoSuchBubble(bubble_id).into());
                }
            }
        }

        Ok(Bubble::new(
            bubble_id,
            expires_at,
            self.blobstore.clone(),
            self.connections.clone(),
            expiry_status,
            labels_lazy,
        ))
    }

    async fn open_bubble(&self, ctx: &CoreContext, bubble_id: BubbleId) -> Result<Bubble> {
        self.open_bubble_raw(ctx, bubble_id, true).await
    }

    /// Extends the TTL of an ephemeral bubble.
    /// The lifetime remains unchanged if the specified duration is shorter than the current remaining lifetime.
    /// If no duration is provided, the lifetime resets to the greater of the default lifetime or the remaining lifetime.
    /// Returns an error if the bubble has expired.
    async fn extend_bubble_ttl(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        custom_duration: Option<Duration>,
    ) -> Result<ExtendBubbleTtlOutcome> {
        // First, get the current bubble information to check its current expiration
        // This will fail if the bubble has expired, which is the desired behavior
        let bubble = self.open_bubble_raw(ctx, bubble_id, true).await?;
        let current_expires_at = bubble.expires_at();

        // Note: We need to subtract the grace period because the database stores the original expiration time
        // without grace period, but bubble.expires_at() includes the grace period
        let db_current_expires_at = current_expires_at - self.bubble_expiration_grace;

        // Calculate the new expiration time
        let now = DateTime::now();
        let new_db_expires_at = match custom_duration {
            None => {
                // No duration provided: reset to the greater of default lifetime or remaining lifetime
                let default_expires_at = now + self.initial_bubble_lifespan;
                default_expires_at.max(db_current_expires_at)
            }
            Some(duration) => {
                // Only extend if the requested duration is longer than the current remaining lifetime
                let requested_expires_at = now + to_chrono(duration);
                // Only extend if the new expiration is later than the current one
                if requested_expires_at > db_current_expires_at {
                    requested_expires_at
                } else {
                    // Don't change the expiration if the requested duration is shorter
                    db_current_expires_at
                }
            }
        };

        // Only perform the database update if the new expiration time is different from the current one
        if new_db_expires_at != db_current_expires_at {
            // This query will only update active bubbles (expired = 0)
            ctx.perf_counters()
                .increment_counter(PerfCounterType::SqlWrites);
            let result = UpdateBubbleExpiration::query(
                &self.connections.write_connection,
                ctx.sql_query_telemetry(),
                &Timestamp::from(new_db_expires_at),
                &bubble_id,
            )
            .await?;

            // Check if any rows were actually updated
            if result.affected_rows() == 0 {
                // No rows were updated, which means the bubble was already expired/deleted
                return Err(EphemeralBlobstoreError::NoSuchBubble(bubble_id).into());
            }

            // TTL was extended
            Ok(ExtendBubbleTtlOutcome::Extended(Timestamp::from(
                new_db_expires_at,
            )))
        } else {
            // TTL was not changed
            Ok(ExtendBubbleTtlOutcome::NotChanged(Timestamp::from(
                db_current_expires_at,
            )))
        }
    }
}

impl RepoEphemeralStore {
    pub(crate) fn new(
        repo_id: RepositoryId,
        connections: SqlConnections,
        blobstore: Arc<dyn BlobstoreEnumerableWithUnlink>,
        initial_bubble_lifespan: Duration,
        bubble_expiration_grace: Duration,
        bubble_deletion_mode: BubbleDeletionMode,
    ) -> Self {
        Self {
            repo_id,
            inner: Some(Arc::new(RepoEphemeralStoreInner {
                blobstore,
                connections,
                initial_bubble_lifespan: to_chrono(initial_bubble_lifespan),
                bubble_expiration_grace: to_chrono(bubble_expiration_grace),
                bubble_deletion_mode,
            })),
        }
    }

    pub fn disabled(repo_id: RepositoryId) -> Self {
        Self {
            inner: None,
            repo_id,
        }
    }

    fn inner(&self) -> Result<&RepoEphemeralStoreInner, EphemeralBlobstoreError> {
        self.inner
            .as_deref()
            .ok_or(EphemeralBlobstoreError::NoEphemeralBlobstore(self.repo_id))
    }

    pub async fn create_bubble(
        &self,
        ctx: &CoreContext,
        custom_duration: Option<Duration>,
        labels: Vec<String>,
    ) -> Result<Bubble> {
        self.inner()?
            .create_bubble(ctx, custom_duration, labels)
            .await
    }

    /// Method responsible for deleting the bubble and all the data contained within.
    /// Returns the number of blobs deleted from the bubble and the number of blobs
    /// that were already deleted.
    /// NOTE: Deletes the bubble regardless of its expiry status. Make sure the bubble
    /// is suitable for deletion.
    pub async fn delete_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
    ) -> Result<DeleteBubbleOutcome> {
        let unlinker = Unlinker::new(UnlinkPolicy::default());
        self.delete_bubble_with_unlinker(ctx, bubble_id, &unlinker)
            .await
    }

    async fn delete_bubble_with_unlinker(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        unlinker: &Unlinker,
    ) -> Result<DeleteBubbleOutcome> {
        self.inner()?.delete_bubble(ctx, bubble_id, unlinker).await
    }

    /// Deletes the given bubbles, running up to `concurrency` deletions at a time,
    /// and yields each bubble's outcome as soon as its deletion finishes.
    /// NOTE: Deletes the bubbles regardless of their expiry status. Make sure the
    /// bubbles are suitable for deletion.
    pub fn delete_bubbles(
        &self,
        ctx: &CoreContext,
        bubble_ids: Vec<BubbleId>,
        concurrency: usize,
        unlink_policy: UnlinkPolicy,
    ) -> impl Stream<Item = (BubbleId, Result<DeleteBubbleOutcome>)> + Send + 'static {
        let store = self.clone();
        let ctx = ctx.clone();
        let unlinker = Arc::new(Unlinker::new(unlink_policy));
        stream::iter(bubble_ids)
            .map(move |bubble_id| {
                let store = store.clone();
                let ctx = ctx.clone();
                let unlinker = unlinker.clone();
                async move {
                    let outcome = store
                        .delete_bubble_with_unlinker(&ctx, bubble_id, &unlinker)
                        .await;
                    (bubble_id, outcome)
                }
            })
            .buffer_unordered(concurrency)
    }

    /// Gets the vector of bubbles that are past their expiry period
    /// by at least a duration of expiry_offset + bubble_expiration_grace
    pub async fn get_expired_bubbles(
        &self,
        ctx: &CoreContext,
        expiry_offset: Duration,
        max_bubbles: u32,
    ) -> Result<Vec<BubbleId>> {
        self.inner()?
            .get_expired_bubbles(ctx, expiry_offset, max_bubbles)
            .await
    }

    /// Gets the blob keys stored within the bubble, optionally starting
    /// from 'start_from' and upto 'max' in count.
    pub async fn keys_in_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        start_from: Option<String>,
        max: u32,
    ) -> Result<Vec<String>> {
        self.inner()?
            .keys_in_bubble(ctx, bubble_id, start_from, max)
            .await
    }

    /// Open the bubble corresponding to the given bubble ID if the bubble
    /// exists and has not yet expired.
    pub async fn open_bubble(&self, ctx: &CoreContext, bubble_id: BubbleId) -> Result<Bubble> {
        self.inner()?.open_bubble(ctx, bubble_id).await
    }

    /// Open the bubble corresponding to the given bubble ID regardless
    /// of the expiry status or date.
    /// NOTE: To be used only for debugging, use open_bubble for other
    /// production use cases.
    pub async fn open_bubble_raw(&self, ctx: &CoreContext, bubble_id: BubbleId) -> Result<Bubble> {
        self.inner()?.open_bubble_raw(ctx, bubble_id, false).await
    }

    /// Given a changeset ID, fetch the corresponding bubble ID.
    pub async fn bubble_from_changeset(
        &self,
        ctx: &CoreContext,
        cs_id: &ChangesetId,
    ) -> Result<Option<BubbleId>> {
        self.inner()?
            .bubble_from_changeset(ctx, &self.repo_id, cs_id)
            .await
    }

    pub async fn bubble_from_hg_changeset(
        &self,
        ctx: &CoreContext,
        hg_cs_id: HgChangesetId,
    ) -> Result<Option<BubbleId>> {
        self.inner()?
            .bubble_from_hg_changeset(ctx, self.repo_id, hg_cs_id)
            .await
    }

    /// Given a bubble ID, fetch the corresponding changeset ID within the
    /// repository associated with the bubble.
    pub async fn changesets_from_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: &BubbleId,
    ) -> Result<Vec<ChangesetId>> {
        self.inner()?.changesets_from_bubble(ctx, bubble_id).await
    }

    /// Associate the given labels with the bubble corresponding to the input
    /// bubble ID.
    pub async fn add_bubble_labels(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        labels: Vec<String>,
    ) -> Result<()> {
        self.inner()?
            .add_bubble_labels(ctx, bubble_id, labels)
            .await
    }

    /// Disassociate the given labels from the bubble corresponding to the input
    /// bubble ID.
    pub async fn remove_bubble_labels(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        labels: Vec<String>,
    ) -> Result<()> {
        self.inner()?
            .remove_bubble_labels(ctx, bubble_id, labels)
            .await
    }

    /// Given a bubble ID, fetches the labels corresponding to that bubble.
    pub async fn labels_from_bubble(
        &self,
        ctx: &CoreContext,
        bubble_id: &BubbleId,
    ) -> Result<Vec<String>> {
        self.inner()?.labels_from_bubble(ctx, bubble_id).await
    }

    /// Extends the TTL of an ephemeral bubble.
    /// The lifetime remains unchanged if the specified duration is shorter than the current remaining lifetime.
    /// If no duration is provided, the lifetime resets to the greater of the default lifetime or the remaining lifetime.
    pub async fn extend_bubble_ttl(
        &self,
        ctx: &CoreContext,
        bubble_id: BubbleId,
        custom_duration: Option<Duration>,
    ) -> Result<ExtendBubbleTtlOutcome> {
        self.inner()?
            .extend_bubble_ttl(ctx, bubble_id, custom_duration)
            .await
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::fmt;
    use std::num::NonZeroU32;
    use std::num::NonZeroUsize;
    use std::sync::Mutex;
    use std::time::Instant;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use blobstore::Blobstore;
    use blobstore::BlobstoreBytes;
    use blobstore::BlobstoreEnumerableWithUnlink;
    use blobstore::BlobstoreEnumerationData;
    use blobstore::BlobstoreGetData;
    use blobstore::BlobstoreIsPresent;
    use blobstore::BlobstoreKeyParam;
    use blobstore::BlobstoreKeySource;
    use blobstore::KeyedBlobstore;
    use blobstore::OverwriteStatus;
    use blobstore::PutBehaviour;
    use bonsai_hg_mapping::BonsaiHgMappingEntry;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use maplit::hashset;
    use memblob::Memblob;
    use mercurial_types_mocks::nodehash as hg;
    use metaconfig_types::BubbleDeletionMode;
    use metaconfig_types::PackFormat;
    use mononoke_macros::mononoke;
    use mononoke_types_mocks::changesetid as bonsai;
    use mononoke_types_mocks::repo::REPO_ONE;
    use mononoke_types_mocks::repo::REPO_ZERO;
    use packblob::PackBlob;
    use repo_blobstore::RepoBlobstore;
    use scuba_ext::MononokeScubaSampleBuilder;
    use sql_construct::SqlConstruct;

    use super::*;
    use crate::builder::RepoEphemeralStoreBuilder;
    use crate::unlink::UnlinkRetry;

    mononoke_queries! {
        read CountBubbleHgMappingRows(bubble_id: BubbleId) -> (u64) {
            "SELECT COUNT(*) FROM ephemeral_bubble_hg_mapping WHERE bubble_id = {bubble_id}"
        }
    }

    async fn seed_hg_mapping(
        ctx: &CoreContext,
        eph: &RepoEphemeralStore,
    ) -> Result<(BubbleId, BubbleId)> {
        let connections = &eph.inner()?.connections;
        let mapping = SqlBubbleHgMapping::new(connections.clone(), REPO_ZERO);
        let a = eph.create_bubble(ctx, None, vec![]).await?.bubble_id();
        let b = eph.create_bubble(ctx, None, vec![]).await?.bubble_id();
        mapping
            .add(
                ctx,
                a,
                &[
                    BonsaiHgMappingEntry {
                        hg_cs_id: hg::ONES_CSID,
                        bcs_id: bonsai::ONES_CSID,
                    },
                    BonsaiHgMappingEntry {
                        hg_cs_id: hg::TWOS_CSID,
                        bcs_id: bonsai::TWOS_CSID,
                    },
                ],
            )
            .await?;
        mapping
            .add(
                ctx,
                b,
                &[BonsaiHgMappingEntry {
                    hg_cs_id: hg::THREES_CSID,
                    bcs_id: bonsai::THREES_CSID,
                }],
            )
            .await?;
        Ok((a, b))
    }

    async fn bubble_with_hg(
        ctx: &CoreContext,
        eph: &RepoEphemeralStore,
        hg_cs_id: HgChangesetId,
        bcs_id: ChangesetId,
    ) -> Result<BubbleId> {
        let bubble_id = eph.create_bubble(ctx, None, vec![]).await?.bubble_id();
        SqlBubbleHgMapping::new(eph.inner()?.connections.clone(), REPO_ZERO)
            .add(ctx, bubble_id, &[BonsaiHgMappingEntry { hg_cs_id, bcs_id }])
            .await?;
        Ok(bubble_id)
    }

    #[mononoke::fbinit_test]
    async fn bubble_from_hg_changeset_test(fb: FacebookInit) -> Result<()> {
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::from_hours(24),
            Duration::from_hours(6),
            BubbleDeletionMode::MarkAndDelete,
        )?;
        let (a, b) = seed_hg_mapping(&ctx, &eph).await?;

        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::TWOS_CSID).await?,
            Some(a),
            "an hg changeset is found in the bubble that holds it"
        );
        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::THREES_CSID).await?,
            Some(b),
            "each hg changeset is found in its own bubble"
        );
        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::FOURS_CSID).await?,
            None,
            "an hg changeset that is in no bubble is not found"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn bubble_from_hg_changeset_is_scoped_by_repo_test(fb: FacebookInit) -> Result<()> {
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::from_hours(24),
            Duration::from_hours(6),
            BubbleDeletionMode::MarkAndDelete,
        )?;
        let bubble_id = eph.create_bubble(&ctx, None, vec![]).await?.bubble_id();
        SqlBubbleHgMapping::new(eph.inner()?.connections.clone(), REPO_ONE)
            .add(
                &ctx,
                bubble_id,
                &[BonsaiHgMappingEntry {
                    hg_cs_id: hg::ONES_CSID,
                    bcs_id: bonsai::ONES_CSID,
                }],
            )
            .await?;

        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::ONES_CSID).await?,
            None,
            "a row of another repo is not visible"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn bubble_from_hg_changeset_skips_expired_test(fb: FacebookInit) -> Result<()> {
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::from_hours(24),
            grace,
            BubbleDeletionMode::MarkAndDelete,
        )?;
        let marked = bubble_with_hg(&ctx, &eph, hg::ONES_CSID, bonsai::ONES_CSID).await?;
        let timed_out = bubble_with_hg(&ctx, &eph, hg::TWOS_CSID, bonsai::TWOS_CSID).await?;
        let in_grace = bubble_with_hg(&ctx, &eph, hg::THREES_CSID, bonsai::THREES_CSID).await?;
        let labeled = bubble_with_hg(&ctx, &eph, hg::FOURS_CSID, bonsai::FOURS_CSID).await?;
        let labeled_marked = bubble_with_hg(&ctx, &eph, hg::FIVES_CSID, bonsai::FIVES_CSID).await?;
        for bubble_id in [labeled, labeled_marked] {
            eph.add_bubble_labels(&ctx, bubble_id, vec!["workspace".to_string()])
                .await?;
        }
        let connections = &eph.inner()?.connections;
        for bubble_id in [marked, labeled_marked] {
            UpdateExpired::query(
                &connections.write_connection,
                ctx.sql_query_telemetry(),
                &ExpiryStatus::Expired,
                &bubble_id,
            )
            .await?;
        }
        let past_grace =
            Timestamp::from(DateTime::now() - to_chrono(grace) - ChronoDuration::minutes(1));
        for bubble_id in [timed_out, labeled, labeled_marked] {
            UpdateBubbleExpiration::query(
                &connections.write_connection,
                ctx.sql_query_telemetry(),
                &past_grace,
                &bubble_id,
            )
            .await?;
        }
        UpdateBubbleExpiration::query(
            &connections.write_connection,
            ctx.sql_query_telemetry(),
            &Timestamp::from(DateTime::now() - ChronoDuration::minutes(1)),
            &in_grace,
        )
        .await?;

        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::ONES_CSID).await?,
            None,
            "a bubble marked expired is skipped"
        );
        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::TWOS_CSID).await?,
            None,
            "a bubble past its grace period is skipped"
        );
        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::THREES_CSID).await?,
            Some(in_grace),
            "a bubble within its grace period is still found"
        );
        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::FOURS_CSID).await?,
            Some(labeled),
            "a labeled bubble past its expiry is still alive"
        );
        assert_eq!(
            eph.bubble_from_hg_changeset(&ctx, hg::FIVES_CSID).await?,
            None,
            "a labeled bubble that is marked expired is skipped"
        );
        Ok(())
    }

    async fn count_hg_mapping_rows(
        ctx: &CoreContext,
        eph: &RepoEphemeralStore,
        bubble_id: BubbleId,
    ) -> Result<u64> {
        let rows = CountBubbleHgMappingRows::query(
            &eph.inner()?.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &bubble_id,
        )
        .await?;
        Ok(rows.first().map_or(0, |row| row.0))
    }

    #[mononoke::fbinit_test]
    async fn delete_bubble_clears_hg_mapping_test(fb: FacebookInit) -> Result<()> {
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::from_hours(24),
            Duration::from_hours(6),
            BubbleDeletionMode::MarkAndDelete,
        )?;
        let (a, b) = seed_hg_mapping(&ctx, &eph).await?;
        assert_eq!(count_hg_mapping_rows(&ctx, &eph, a).await?, 2);

        eph.delete_bubble(&ctx, a).await?;

        assert_eq!(
            count_hg_mapping_rows(&ctx, &eph, a).await?,
            0,
            "both rows of the deleted bubble are gone"
        );
        assert_eq!(
            count_hg_mapping_rows(&ctx, &eph, b).await?,
            1,
            "another bubble keeps its rows"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn mark_only_keeps_hg_mapping_test(fb: FacebookInit) -> Result<()> {
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::from_hours(24),
            Duration::from_hours(6),
            BubbleDeletionMode::MarkOnly,
        )?;
        let (a, _) = seed_hg_mapping(&ctx, &eph).await?;

        eph.delete_bubble(&ctx, a).await?;

        assert_eq!(
            count_hg_mapping_rows(&ctx, &eph, a).await?,
            2,
            "MarkOnly keeps the rows"
        );
        assert!(
            eph.open_bubble(&ctx, a).await.is_err(),
            "the rows are unreachable because the bubble is marked expired"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubble_retry_is_safe_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(24);
        let grace = Duration::from_hours(6);
        let (ctx, blobstore, _, mark_only) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;
        let (a, b) = seed_hg_mapping(&ctx, &mark_only).await?;
        let mark_and_delete = RepoEphemeralStore::new(
            REPO_ZERO,
            mark_only.inner()?.connections.clone(),
            blobstore,
            initial,
            grace,
            BubbleDeletionMode::MarkAndDelete,
        );

        mark_only.delete_bubble(&ctx, a).await?;
        mark_and_delete.delete_bubble(&ctx, a).await?;

        assert_eq!(count_hg_mapping_rows(&ctx, &mark_and_delete, a).await?, 0);
        assert_eq!(count_hg_mapping_rows(&ctx, &mark_and_delete, b).await?, 1);
        Ok(())
    }

    fn bootstrap(
        fb: FacebookInit,
        initial_lifespan: Duration,
        grace_period: Duration,
        deletion_mode: BubbleDeletionMode,
    ) -> Result<(
        CoreContext,
        Arc<dyn BlobstoreEnumerableWithUnlink>,
        RepoBlobstore,
        RepoEphemeralStore,
    )> {
        let ctx = CoreContext::test_mock(fb);
        // The ephemeral blobstore will normally be used stacked on top of
        // packblob, so use this in the test, too.
        let blobstore = Arc::new(PackBlob::new(
            Memblob::default(),
            PackFormat::ZstdIndividual(0),
        )) as Arc<dyn BlobstoreEnumerableWithUnlink>;
        let repo_blobstore = RepoBlobstore::new(
            Arc::new(Memblob::default()),
            None,
            REPO_ZERO,
            MononokeScubaSampleBuilder::with_discard(),
        );
        let eph = RepoEphemeralStoreBuilder::with_sqlite_in_memory()?.build(
            REPO_ZERO,
            blobstore.clone(),
            initial_lifespan,
            grace_period,
            deletion_mode,
        );
        Ok((ctx, blobstore, repo_blobstore, eph))
    }

    #[mononoke::fbinit_test]
    async fn basic_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, blobstore, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let key = "test_key".to_string();

        // Create a bubble and put data in it.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble1_id = bubble1.bubble_id();
        let bubble1 = bubble1.wrap_repo_blobstore(repo_blobstore.clone());
        bubble1
            .put(&ctx, key.clone(), BlobstoreBytes::from_bytes("test data"))
            .await?;
        let data = bubble1.get(&ctx, &key).await?.unwrap().into_bytes();
        assert_eq!(data.as_bytes().as_ref(), b"test data");

        // Re-open the bubble and confirm we can read the data.
        let bubble1_read = eph
            .open_bubble(&ctx, bubble1_id)
            .await?
            .wrap_repo_blobstore(repo_blobstore.clone());
        let data = bubble1_read.get(&ctx, &key).await?.unwrap().into_bytes();
        assert_eq!(data.as_bytes().as_ref(), b"test data");

        // Enumerate the blobstore and check the key got its prefix.
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert_eq!(
            enumerated.keys,
            hashset! { format!("eph{bubble1_id}.repo0000.{key}") }
        );

        // Create a new bubble and put data in it.
        let bubble2 = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble2_id = bubble2.bubble_id();
        let bubble2 = bubble2.wrap_repo_blobstore(repo_blobstore.clone());
        bubble2
            .put(
                &ctx,
                key.clone(),
                BlobstoreBytes::from_bytes("other test data"),
            )
            .await?;
        let data = bubble2.get(&ctx, &key).await?.unwrap().into_bytes();
        assert_eq!(data.as_bytes().as_ref(), b"other test data");

        let data = bubble1.get(&ctx, &key).await?.unwrap().into_bytes();
        assert_eq!(data.as_bytes().as_ref(), b"test data");

        // There should now be two separate keys.
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert_eq!(
            enumerated.keys,
            hashset! {
                format!("eph{bubble1_id}.repo0000.{key}"),
                format!("eph{}.repo0000.{}", bubble2_id, key),
            }
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn basic_test_with_labels(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        // Create a bubble with labels associated to it.
        let bubble1 = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Verify all the labels are associated with the bubble.
        assert!(
            bubble1
                .labels()
                .await?
                .iter()
                .zip(&labels)
                .filter(|&(l, r)| l == r)
                .count()
                == labels.len()
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Ensure that the bubble is created with no labels.
        assert!(bubble.labels().await?.is_empty());
        // Add labels to the newly created bubble.
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        eph.add_bubble_labels(&ctx, bubble.bubble_id(), labels.clone())
            .await?;
        // Reopen bubble from the store and verify it has the added labels.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?.len(), labels.len());
        assert!(
            bubble_read
                .labels()
                .await?
                .iter()
                .all(|label| labels.contains(label))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_duplicate_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Ensure that the bubble is created with no labels.
        assert!(bubble.labels().await?.is_empty());
        // Add unique + duplicate labels to the newly created bubble.
        let labels = vec![
            "workspace".to_string(),
            "debug_version".to_string(),
            "workspace".to_string(),
            "workspace".to_string(),
        ];
        eph.add_bubble_labels(&ctx, bubble.bubble_id(), labels)
            .await?;
        // Reopen bubble from the store and verify it has the added labels
        // and the labels are not duplicated.
        let unique_labels = ["workspace".to_string(), "debug_version".to_string()];
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?.len(), unique_labels.len());
        assert!(
            bubble_read
                .labels()
                .await?
                .iter()
                .all(|label| unique_labels.contains(label))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_empty_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Ensure that the bubble is created with no labels.
        assert!(bubble.labels().await?.is_empty());
        // Add an empty vec of labels.
        let labels = vec![];
        eph.add_bubble_labels(&ctx, bubble.bubble_id(), labels)
            .await?;
        // Reopen bubble from the store and verify it still doesn't
        // have any labels since we used an empty vec of labels as input.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert!(bubble_read.labels().await?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn empty_labels_from_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with no labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Fetch the labels associated with the newly created bubble.
        let id = bubble.bubble_id();
        let labels = eph.labels_from_bubble(&ctx, &id).await?;
        // Validate that no labels are associated with the bubble.
        assert!(labels.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn non_empty_labels_from_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let labels = vec!["workspace".to_string(), "debug".to_string()];
        // Create a bubble with some labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Fetch the labels associated with the newly created bubble.
        let id = bubble.bubble_id();
        let returned_labels = eph.labels_from_bubble(&ctx, &id).await?;
        // Validate that the labels returned are the same as the stored labels.
        assert_eq!(returned_labels.len(), labels.len());
        assert!(returned_labels.iter().all(|label| labels.contains(label)));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn added_removed_labels_from_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with no labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Add labels to the newly created bubble.
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        eph.add_bubble_labels(&ctx, bubble.bubble_id(), labels.clone())
            .await?;
        // Fetch the labels associated with the bubble.
        let id = bubble.bubble_id();
        let returned_labels = eph.labels_from_bubble(&ctx, &id).await?;
        // Validate that the labels returned are the same as the added labels.
        assert_eq!(returned_labels.len(), labels.len());
        assert!(returned_labels.iter().all(|label| labels.contains(label)));
        // Remove all labels associated with the bubble.
        eph.remove_bubble_labels(&ctx, bubble.bubble_id(), labels)
            .await?;
        let returned_labels = eph.labels_from_bubble(&ctx, &id).await?;
        // Validate that no labels are returned since all labels were deleted.
        assert!(returned_labels.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn remove_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        // Create a bubble with labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Reopen bubble and verify all the labels are associated with the bubble.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?.len(), labels.len());
        assert!(
            bubble_read
                .labels()
                .await?
                .iter()
                .all(|label| labels.contains(label))
        );
        // Remove all labels associated with the bubble.
        eph.remove_bubble_labels(&ctx, bubble.bubble_id(), labels)
            .await?;
        // Reopen the bubble and validate that it has no labels associated with it.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert!(bubble_read.labels().await?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn partial_remove_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let bubble = eph
            .create_bubble(
                &ctx,
                None,
                vec!["workspace".to_string(), "debug_version".to_string()],
            )
            .await?;
        // Remove a subset of the labels associated with the bubble.
        eph.remove_bubble_labels(&ctx, bubble.bubble_id(), vec!["workspace".to_string()])
            .await?;
        // Reopen the bubble and validate it has the remaining labels associated with it.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(
            bubble_read.labels().await?,
            vec!["debug_version".to_string()]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn remove_absent_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Remove labels that are not part of the bubble. This should be a no-op
        // from the bubble's perspective.
        eph.remove_bubble_labels(
            &ctx,
            bubble.bubble_id(),
            vec!["some_random_label".to_string()],
        )
        .await?;
        // Reopen the bubble and validate it has the same labels as before since the
        // no existing labels were deleted.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?.len(), labels.len());
        assert!(
            bubble_read
                .labels()
                .await?
                .iter()
                .all(|label| labels.contains(label))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn remove_duplicate_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Remove labels that are not part of the bubble. This should be a no-op
        // from the bubble's perspective.
        eph.remove_bubble_labels(
            &ctx,
            bubble.bubble_id(),
            vec![
                "workspace".to_string(),
                "workspace".to_string(),
                "workspace".to_string(),
            ],
        )
        .await?;
        // Reopen the bubble and validate it has only "debug_version" label since
        // the other label was deleted. Passing input vec with duplicate labels
        // should not have any other effect.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(
            bubble_read.labels().await?,
            vec!["debug_version".to_string()]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn remove_all_bubble_labels_without_input_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Remove bubble labels without specifying the exact labels to remove.
        eph.remove_bubble_labels(&ctx, bubble.bubble_id(), Vec::new())
            .await?;
        // Reopen the bubble and validate that all the labels have been removed.
        // Calling remove_bubble_labels without any input labels should remove
        // all the associated labels.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?, Vec::<String>::new());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn remove_all_bubble_labels_with_empty_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with no labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, Vec::new()).await?;
        // Remove bubble labels without specifying the exact labels to remove.
        // Even though no labels are associated with the bubble, operation of
        // removing all labels still succeeds as a no-op.
        eph.remove_bubble_labels(&ctx, bubble.bubble_id(), Vec::new())
            .await?;
        // Reopen the bubble and validate that the state of labels is the
        // same as before
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?, Vec::<String>::new());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_and_remove_bubble_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble with labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Ensure that the bubble is created with no labels.
        assert!(bubble.labels().await?.is_empty());
        // Add labels to the newly created bubble.
        let labels = vec![
            "workspace".to_string(),
            "debug_version".to_string(),
            "some_label".to_string(),
            "important_snapshot".to_string(),
        ];
        eph.add_bubble_labels(&ctx, bubble.bubble_id(), labels)
            .await?;
        // Remove a partial subset of the labels associated to the bubble.
        eph.remove_bubble_labels(
            &ctx,
            bubble.bubble_id(),
            vec!["workspace".to_string(), "debug_version".to_string()],
        )
        .await?;
        // Reopen the bubble and verify that it has the right set of labels after the
        // add and remove operation.
        let remaining_labels = ["some_label".to_string(), "important_snapshot".to_string()];
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        assert_eq!(bubble_read.labels().await?.len(), remaining_labels.len());
        assert!(
            bubble_read
                .labels()
                .await?
                .iter()
                .all(|label| remaining_labels.contains(label))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn create_and_fetch_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let labels = vec!["workspace".to_string(), "debug_version".to_string()];
        // Create a bubble with labels associated to it.
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Re-opening the bubble from storage.
        let bubble_read = eph.open_bubble(&ctx, bubble.bubble_id()).await?;
        // Validate that the labels in the stored bubble and the retrieved bubble are the same.
        let bubble_labels = bubble.labels().await?;
        let bubble_read_labels = bubble_read.labels().await?;
        assert_eq!(bubble_labels.len(), bubble_read_labels.len());
        assert!(
            bubble_labels
                .iter()
                .all(|label| bubble_read_labels.contains(label))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn create_and_fetch_active_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        // Ensure a newly created bubble exists in Active status
        assert_eq!(bubble1.expired(), ExpiryStatus::Active);
        // Re-opening the bubble from storage returns the same status
        let bubble1_read = eph.open_bubble(&ctx, bubble1.bubble_id()).await?;
        assert_eq!(bubble1_read.expired(), bubble1.expired());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn deletion_mode_disabled_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        // We want an ephemeral store where deletion is disabled.
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::Disabled)?;
        // Create an empty bubble.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        // Attempt to delete the bubble
        let res = eph.delete_bubble(&ctx, bubble1.bubble_id()).await;
        // Since the bubble is deleted, reopening the bubble should
        // throw the "no such bubble" error
        match res {
            Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
                Some(EphemeralBlobstoreError::DeleteBubbleDisabled) => Ok(()),
                _ => Err(anyhow!("Invalid error during bubble deletion")),
            },
            _ => Err(anyhow!(
                "Bubble deletion should be disabled but it is enabled"
            )),
        }
    }

    #[mononoke::fbinit_test]
    async fn deletion_mode_markonly_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        // We want an ephemeral store where deletion mode is mark only.
        let (ctx, blobstore, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;
        // Create a bubble and add data to it.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble1_repo = bubble1.wrap_repo_blobstore(repo_blobstore.clone());
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_1"),
                BlobstoreBytes::from_bytes("test data 1"),
            )
            .await?;
        // Add more data to it
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_2"),
                BlobstoreBytes::from_bytes("test data 2"),
            )
            .await?;
        // Enumerate the blobstore and check the required data is present.
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        // Should contain two keys for now
        assert_eq!(enumerated.keys.len(), 2);
        // Delete the bubble
        let deleted = eph.delete_bubble(&ctx, bubble1.bubble_id()).await?;
        // Should be 0 since deletion mode is MarkOnly
        assert_eq!(deleted.keys_deleted, 0);
        // Even though the bubble hasn't been deleted from the backing physical store
        // it should appear as deleted to the user and hence not be accessible.
        let res = eph.open_bubble(&ctx, bubble1.bubble_id()).await;
        // Since the bubble is deleted, reopening the bubble should
        // throw the "no such bubble" error
        match res {
            Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
                Some(EphemeralBlobstoreError::NoSuchBubble(_)) => Ok(()),
                _ => Err(anyhow!("Invalid error post bubble deletion")),
            },
            _ => Err(anyhow!(
                "Bubble expected to be (soft) deleted but it still exists for the user"
            )),
        }
    }

    #[mononoke::fbinit_test]
    async fn delete_empty_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        // Delete the bubble
        let deleted = eph.delete_bubble(&ctx, bubble1.bubble_id()).await?;
        // Should be 0 since the bubble was empty
        assert_eq!(deleted.keys_deleted, 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_empty_bubble_with_label_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble with labels.
        let labels = vec!["workspace".to_string(), "test".to_string()];
        let bubble1 = eph.create_bubble(&ctx, None, labels).await?;
        // Delete the bubble
        let deleted = eph.delete_bubble(&ctx, bubble1.bubble_id()).await?;
        // Should be 0 since the bubble was empty
        assert_eq!(deleted.keys_deleted, 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_nonempty_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, blobstore, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble and add data to it.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble1_repo = bubble1.wrap_repo_blobstore(repo_blobstore.clone());
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_1"),
                BlobstoreBytes::from_bytes("test data 1"),
            )
            .await?;
        // Add more data to it
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_2"),
                BlobstoreBytes::from_bytes("test data 2"),
            )
            .await?;
        // Add some more data to it
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_3"),
                BlobstoreBytes::from_bytes("test data 3"),
            )
            .await?;
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_4"),
                BlobstoreBytes::from_bytes("test data 4"),
            )
            .await?;
        // Enumerate the blobstore and check the required data is present.
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        // Should contain four keys for now
        assert_eq!(enumerated.keys.len(), 4);
        // Delete the bubble
        let deleted = eph.delete_bubble(&ctx, bubble1.bubble_id()).await?;
        // Should be 4 based on the input data added
        assert_eq!(deleted.keys_deleted, 4);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubbles_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, blobstore, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let empty_bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let nonempty_bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let nonempty_repo = nonempty_bubble.wrap_repo_blobstore(repo_blobstore);
        for key in ["test_key_1", "test_key_2"] {
            nonempty_repo
                .put(
                    &ctx,
                    key.to_string(),
                    BlobstoreBytes::from_bytes("test data"),
                )
                .await?;
        }
        let missing_bubble = BubbleId::new(std::num::NonZeroU64::new(99999).unwrap());

        let mut results = eph
            .delete_bubbles(
                &ctx,
                vec![
                    empty_bubble.bubble_id(),
                    nonempty_bubble.bubble_id(),
                    missing_bubble,
                ],
                2,
                UnlinkPolicy::default(),
            )
            .collect::<Vec<_>>()
            .await;
        results.sort_by_key(|(bubble_id, _)| *bubble_id);

        let [(_, empty), (_, nonempty), (_, missing)] = results.as_slice() else {
            panic!(
                "Every requested bubble should be reported, got {} results",
                results.len()
            );
        };
        assert!(
            matches!(empty, Ok(outcome) if outcome.keys_deleted == 0),
            "The empty bubble should be deleted without deleting any keys"
        );
        assert!(
            matches!(nonempty, Ok(outcome) if outcome.keys_deleted == 2),
            "Both keys of the non-empty bubble should be deleted"
        );
        assert!(
            missing.is_err(),
            "Deleting an unknown bubble should fail without affecting the others"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert!(enumerated.keys.is_empty(), "No bubble data should remain");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn reopen_deleted_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, _, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create a bubble and add data to it.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble1_repo = bubble1.wrap_repo_blobstore(repo_blobstore.clone());
        bubble1_repo
            .put(
                &ctx,
                String::from("test_key_1"),
                BlobstoreBytes::from_bytes("test data 1"),
            )
            .await?;
        // Delete the bubble
        eph.delete_bubble(&ctx, bubble1.bubble_id()).await?;
        let res = eph.open_bubble(&ctx, bubble1.bubble_id()).await;
        // Since the bubble is deleted, reopening the bubble should
        // throw the "no such bubble" error
        match res {
            Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
                Some(EphemeralBlobstoreError::NoSuchBubble(_)) => Ok(()),
                _ => Err(anyhow!("Invalid error post bubble deletion")),
            },
            _ => Err(anyhow!("Bubble expected to be deleted but it still exists")),
        }
    }

    #[mononoke::fbinit_test]
    async fn get_expired_bubbles_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble that would expire immediately.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        // Validate bubble is created in active state
        assert_eq!(bubble1.expired(), ExpiryStatus::Active);
        // Create an empty bubble that won't expire anytime soon.
        let bubble2 = eph
            .create_bubble(&ctx, Some(Duration::from_secs(10000)), vec![])
            .await?;
        // Validate bubble is created in active state
        assert_eq!(bubble2.expired(), ExpiryStatus::Active);
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 10)
            .await?;
        // Only one bubble should be returned since only one has expired so far
        assert_eq!(res.len(), 1);
        let res_bubble_id = res
            .first()
            .expect("Invalid number of expired bubbles")
            .clone();
        // The first bubble should be the only one that's expired
        assert_eq!(res_bubble_id, bubble1.bubble_id());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn get_expired_bubbles_with_labels_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble that would expire immediately but has labels
        // associated with it.
        let labels = vec!["workspace".to_string(), "test".to_string()];
        eph.create_bubble(&ctx, None, labels).await?;
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 10)
            .await?;
        // Even though there exists one bubble beyond its expiry time, it should
        // not be returned since it has labels associated with it.
        assert_eq!(res.len(), 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn get_expired_bubbles_with_removed_labels_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble that would expire immediately but has labels
        // associated with it.
        let labels = vec!["workspace".to_string(), "test".to_string()];
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 10)
            .await?;
        // Even though there exists one bubble beyond its expiry time, it should
        // not be returned since it has labels associated with it.
        assert_eq!(res.len(), 0);
        // Remove the labels associated with the bubble.
        eph.remove_bubble_labels(&ctx, bubble.bubble_id(), labels)
            .await?;
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 10)
            .await?;
        // Now that the labels are removed, we should get the above bubble as
        // an expired bubble.
        assert_eq!(res.len(), 1);
        let res_bubble_id = res.first().expect("Invalid number of expired bubbles");
        assert_eq!(res_bubble_id, &bubble.bubble_id());
        let reopened = eph
            .inner()?
            .open_bubble_raw(&ctx, bubble.bubble_id(), false)
            .await?;
        let error = reopened.get(&ctx, "test_key").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<EphemeralBlobstoreError>(),
            Some(EphemeralBlobstoreError::BubbleExpired(_))
        ));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn add_labels_to_expired_bubble_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        // Create in MarkOnly mode since we want to soft delete to test expiration
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;
        // Create an empty bubble that would expire immediately.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Mark bubble as expired (i.e. soft delete)
        eph.delete_bubble(&ctx, bubble.bubble_id()).await?;
        // Add new labels to the bubble. This operation should fail since adding or
        // removing labels from an expired bubble is not permitted.
        let res = eph
            .add_bubble_labels(
                &ctx,
                bubble.bubble_id(),
                vec!["workspace".to_string(), "test".to_string()],
            )
            .await;
        assert!(res.is_err());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn remove_labels_from_expired_bubble_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        // Create in MarkOnly mode since we want to soft delete to test expiration
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;
        // Create an empty bubble that would expire immediately.
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        // Mark bubble as expired (i.e. soft delete)
        eph.delete_bubble(&ctx, bubble.bubble_id()).await?;
        let res = eph
            .remove_bubble_labels(
                &ctx,
                bubble.bubble_id(),
                vec!["workspace".to_string(), "test".to_string()],
            )
            .await;
        assert!(res.is_err());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn get_expired_bubbles_offset_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble that would expire immediately.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        // Validate bubble is created in active state
        assert_eq!(bubble1.expired(), ExpiryStatus::Active);
        // Create an empty bubble that won't expire anytime soon.
        let bubble2 = eph
            .create_bubble(&ctx, Some(Duration::from_secs(10000)), vec![])
            .await?;
        // Validate bubble is created in active state
        assert_eq!(bubble2.expired(), ExpiryStatus::Active);
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(1000), 10)
            .await?;
        // No items should be returned since there aren't any bubbles
        // that have been expired for at least the past 1000 seconds
        assert_eq!(res.len(), 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn get_expired_bubbles_markonly_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        // We want an ephemeral store that only marks the bubbles as expired.
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;
        // Create empty bubbles that would expire immediately.
        eph.create_bubble(&ctx, None, vec![]).await?;
        eph.create_bubble(&ctx, None, vec![]).await?;
        eph.create_bubble(&ctx, None, vec![]).await?;
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 10)
            .await?;
        // All 3 bubbles qualify for expiry and currently have their
        // status as ACTIVE, hence should be present in res
        assert_eq!(res.len(), 3);
        // Delete the bubbles in mark-only mode
        for id in res.into_iter() {
            eph.delete_bubble(&ctx, id).await?;
        }
        // The bubbles should now be soft-deleted but still be present in the
        // backing table. However, get_expired_bubbles should not return those
        // bubbles since their status will be EXPIRED.
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 10)
            .await?;
        assert_eq!(res.len(), 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn get_n_expired_bubbles_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create empty bubbles that would expire immediately.
        eph.create_bubble(&ctx, None, vec![]).await?;
        eph.create_bubble(&ctx, None, vec![]).await?;
        eph.create_bubble(&ctx, None, vec![]).await?;
        eph.create_bubble(&ctx, None, vec![]).await?;
        eph.create_bubble(&ctx, None, vec![]).await?;
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 2)
            .await?;
        // Only 2 bubbles should be returned given the input limit of 2
        assert_eq!(res.len(), 2);
        // Get expired bubbles
        let res = eph
            .get_expired_bubbles(&ctx, Duration::from_secs(0), 0)
            .await?;
        // No bubbles should be returned since limit is 0
        assert_eq!(res.len(), 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn reopen_expired_bubble_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        // Using MarkOnly mode since we want to mark the bubble as expired (Soft Delete)
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;
        // Create an empty bubble that would expire immediately.
        let bubble1 = eph.create_bubble(&ctx, None, vec![]).await?;
        // Mark the bubble as expired
        eph.delete_bubble(&ctx, bubble1.bubble_id()).await?;
        // Opening the expired bubble should give a
        // "No such bubble" error
        let res = eph.open_bubble(&ctx, bubble1.bubble_id()).await;
        match res {
            Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
                Some(EphemeralBlobstoreError::NoSuchBubble(_)) => Ok(()),
                _ => Err(anyhow!("Invalid error post bubble deletion")),
            },
            _ => Err(anyhow!("Bubble expected to be deleted but it still exists")),
        }
    }

    #[mononoke::fbinit_test]
    async fn reopen_expired_bubble_with_labels_test(fb: FacebookInit) -> Result<()> {
        // We want immediately expiring bubbles
        let initial = Duration::from_secs(0);
        let grace = Duration::from_secs(0);
        let (ctx, _, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        // Create an empty bubble with labels that would expire immediately.
        let labels = vec!["workspace".to_string()];
        let bubble1 = eph.create_bubble(&ctx, None, labels).await?;
        let blobstore = bubble1.wrap_repo_blobstore(repo_blobstore.clone());
        blobstore
            .put(
                &ctx,
                "test_key".to_string(),
                BlobstoreBytes::from_bytes("test data"),
            )
            .await?;
        let opened_bubble = eph.open_bubble(&ctx, bubble1.bubble_id()).await?;
        // Bubble should be reopened successfully since even though its expired by time
        // ,having labels associated with it should mark it as active.
        assert_eq!(opened_bubble.bubble_id(), bubble1.bubble_id());
        assert_eq!(opened_bubble.labels().await?, bubble1.labels().await?);
        let blobstore = opened_bubble.wrap_repo_blobstore(repo_blobstore);
        assert!(matches!(
            blobstore.is_present(&ctx, "test_key").await?,
            BlobstoreIsPresent::Present
        ));
        let data = blobstore.get(&ctx, "test_key").await?.unwrap().into_bytes();
        assert_eq!(data.as_bytes().as_ref(), b"test data");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn read_expired_bubble_without_labels_test(fb: FacebookInit) -> Result<()> {
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::ZERO,
            Duration::ZERO,
            BubbleDeletionMode::MarkOnly,
        )?;
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let error = bubble.get(&ctx, "test_key").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<EphemeralBlobstoreError>(),
            Some(EphemeralBlobstoreError::BubbleExpired(_))
        ));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn read_marked_expired_bubble_with_labels_test(fb: FacebookInit) -> Result<()> {
        let (ctx, _, _, eph) = bootstrap(
            fb,
            Duration::from_hours(1),
            Duration::ZERO,
            BubbleDeletionMode::MarkOnly,
        )?;
        let bubble = eph
            .create_bubble(&ctx, None, vec!["workspace".to_string()])
            .await?;
        eph.delete_bubble(&ctx, bubble.bubble_id()).await?;
        let reopened = eph
            .inner()?
            .open_bubble_raw(&ctx, bubble.bubble_id(), false)
            .await?;
        assert_eq!(reopened.labels().await?, vec!["workspace".to_string()]);
        let error = reopened.get(&ctx, "test_key").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<EphemeralBlobstoreError>(),
            Some(EphemeralBlobstoreError::BubbleExpired(_))
        ));
        Ok(())
    }

    // Tests for lifetime extension logic
    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_with_longer_duration_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(1); // 1 minute initial lifespan
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Create a bubble
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();

        // Extend TTL with a longer duration (2 minutes)
        let result = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(2)))
            .await?;

        // Should return Extended with the new timestamp
        match result {
            ExtendBubbleTtlOutcome::Extended(timestamp) => {
                // The new expiration should be approximately 2 minutes from now
                let now = DateTime::now();
                let expected_expiry = now + to_chrono(Duration::from_mins(2));
                let actual_expiry: DateTime = timestamp.into();

                // Allow for some time difference due to test execution time
                let actual_chrono = actual_expiry.into_chrono();
                let expected_chrono = expected_expiry.into_chrono();
                let diff_abs = if actual_chrono > expected_chrono {
                    actual_chrono - expected_chrono
                } else {
                    expected_chrono - actual_chrono
                };
                assert!(
                    diff_abs < to_chrono(Duration::from_secs(5)),
                    "Expiry time should be close to expected"
                );
            }
            ExtendBubbleTtlOutcome::NotChanged(_) => {
                return Err(anyhow!(
                    "Expected TTL to be extended but it was not changed"
                ));
            }
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_with_shorter_duration_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(2); // 2 minutes initial lifespan
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Create a bubble
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();

        // Try to extend TTL with a shorter duration (30 seconds)
        let result = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_secs(30)))
            .await?;

        // Should return NotChanged since the requested duration is shorter than remaining time
        match result {
            ExtendBubbleTtlOutcome::NotChanged(timestamp) => {
                // The timestamp should be approximately the original expiration time
                let now = DateTime::now();
                let expected_expiry = now + to_chrono(Duration::from_mins(2));
                let actual_expiry: DateTime = timestamp.into();

                // Allow for some time difference due to test execution time
                let actual_chrono = actual_expiry.into_chrono();
                let expected_chrono = expected_expiry.into_chrono();
                let diff = if actual_chrono > expected_chrono {
                    actual_chrono - expected_chrono
                } else {
                    expected_chrono - actual_chrono
                };
                assert!(
                    diff < to_chrono(Duration::from_secs(5)),
                    "Expiry time should be close to original"
                );
            }
            ExtendBubbleTtlOutcome::Extended(_) => {
                return Err(anyhow!(
                    "Expected TTL to remain unchanged but it was extended"
                ));
            }
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_without_duration_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(1); // 1 minute initial lifespan
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Create a bubble
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();

        // Wait a bit to let some time pass
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Extend TTL without specifying duration (should reset to default or keep current, whichever is greater)
        let result = eph.extend_bubble_ttl(&ctx, bubble_id, None).await?;

        // Should return Extended since we're resetting to the default lifetime
        match result {
            ExtendBubbleTtlOutcome::Extended(timestamp) => {
                // The new expiration should be approximately 1 minute from now (default lifetime)
                let now = DateTime::now();
                let expected_expiry = now + to_chrono(Duration::from_mins(1));
                let actual_expiry: DateTime = timestamp.into();

                // Allow for some time difference due to test execution time
                let actual_chrono = actual_expiry.into_chrono();
                let expected_chrono = expected_expiry.into_chrono();
                let diff = if actual_chrono > expected_chrono {
                    actual_chrono - expected_chrono
                } else {
                    expected_chrono - actual_chrono
                };
                assert!(
                    diff < to_chrono(Duration::from_secs(5)),
                    "Expiry time should be close to expected"
                );
            }
            ExtendBubbleTtlOutcome::NotChanged(_) => {
                // This could happen if the remaining time is already greater than the default
                // This is also a valid outcome
            }
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_expired_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_secs(0); // Immediately expiring bubble
        let grace = Duration::from_secs(0);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkOnly)?;

        // Create a bubble that expires immediately
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();

        // Mark the bubble as expired
        eph.delete_bubble(&ctx, bubble_id).await?;

        // Try to extend TTL of the expired bubble
        let result = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(1)))
            .await;

        // Should return an error since the bubble is expired
        match result {
            Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
                Some(EphemeralBlobstoreError::NoSuchBubble(_)) => Ok(()),
                _ => Err(anyhow!(
                    "Expected NoSuchBubble error but got different error"
                )),
            },
            Ok(_) => Err(anyhow!(
                "Expected error when extending TTL of expired bubble"
            )),
        }
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_nonexistent_bubble_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(1);
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Try to extend TTL of a non-existent bubble
        let fake_bubble_id = BubbleId::new(std::num::NonZeroU64::new(99999).unwrap());
        let result = eph
            .extend_bubble_ttl(&ctx, fake_bubble_id, Some(Duration::from_mins(1)))
            .await;

        // Should return an error since the bubble doesn't exist
        match result {
            Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
                Some(EphemeralBlobstoreError::NoSuchBubble(_)) => Ok(()),
                _ => Err(anyhow!(
                    "Expected NoSuchBubble error but got different error"
                )),
            },
            Ok(_) => Err(anyhow!(
                "Expected error when extending TTL of non-existent bubble"
            )),
        }
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_with_labels_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(1);
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Create a bubble with labels
        let labels = vec!["test".to_string(), "workspace".to_string()];
        let bubble = eph.create_bubble(&ctx, None, labels.clone()).await?;
        let bubble_id = bubble.bubble_id();

        // Extend TTL with a longer duration
        let result = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(2)))
            .await?;

        // Should return Extended
        match result {
            ExtendBubbleTtlOutcome::Extended(_) => {
                // Verify the bubble can still be opened and has the same labels
                let reopened_bubble = eph.open_bubble(&ctx, bubble_id).await?;
                assert_eq!(reopened_bubble.labels().await?, labels);
            }
            ExtendBubbleTtlOutcome::NotChanged(_) => {
                return Err(anyhow!("Expected TTL to be extended"));
            }
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_multiple_times_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(1);
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Create a bubble
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();

        // First extension
        let result1 = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(2)))
            .await?;
        assert!(matches!(result1, ExtendBubbleTtlOutcome::Extended(_)));

        // Second extension with even longer duration
        let result2 = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(3)))
            .await?;
        assert!(matches!(result2, ExtendBubbleTtlOutcome::Extended(_)));

        // Third extension with shorter duration (should not change)
        let result3 = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(1)))
            .await?;
        assert!(matches!(result3, ExtendBubbleTtlOutcome::NotChanged(_)));

        // Forth extension with even shorter duration (should not change)
        let result4 = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_secs(0)))
            .await?;
        assert!(matches!(result4, ExtendBubbleTtlOutcome::NotChanged(_)));

        // Verify bubble is still accessible
        let reopened_bubble = eph.open_bubble(&ctx, bubble_id).await?;
        assert_eq!(reopened_bubble.bubble_id(), bubble_id);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn extend_bubble_ttl_outcome_timestamps_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_mins(1);
        let grace = Duration::from_secs(30);
        let (ctx, _, _, eph) = bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;

        // Create a bubble
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();

        // Get the original expiration time (approximately)
        let now_before = DateTime::now();

        // Extend with a longer duration
        let result = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_mins(2)))
            .await?;

        match result {
            ExtendBubbleTtlOutcome::Extended(new_timestamp) => {
                let new_expiry: DateTime = new_timestamp.into();
                let expected_expiry = now_before + to_chrono(Duration::from_mins(2));

                // The new expiry should be approximately 2 minutes from the original time
                let new_expiry_chrono = new_expiry.into_chrono();
                let expected_expiry_chrono = expected_expiry.into_chrono();
                let diff = if new_expiry_chrono > expected_expiry_chrono {
                    new_expiry_chrono - expected_expiry_chrono
                } else {
                    expected_expiry_chrono - new_expiry_chrono
                };
                assert!(
                    diff < to_chrono(Duration::from_secs(5)),
                    "New expiry time should be close to expected"
                );
            }
            ExtendBubbleTtlOutcome::NotChanged(_) => {
                return Err(anyhow!("Expected TTL to be extended"));
            }
        }

        // Now try with a shorter duration
        let result2 = eph
            .extend_bubble_ttl(&ctx, bubble_id, Some(Duration::from_secs(30)))
            .await?;

        match result2 {
            ExtendBubbleTtlOutcome::NotChanged(unchanged_timestamp) => {
                // The timestamp should be the same as the previous extension
                if let ExtendBubbleTtlOutcome::Extended(prev_timestamp) = result {
                    assert_eq!(
                        unchanged_timestamp, prev_timestamp,
                        "Timestamp should remain the same when not extended"
                    );
                }
            }
            ExtendBubbleTtlOutcome::Extended(_) => {
                return Err(anyhow!("Expected TTL to remain unchanged"));
            }
        }

        Ok(())
    }

    #[derive(Debug)]
    enum UnlinkFault {
        KeyVanishesAfterListing,
        UnlinkFails,
        UnlinkFailsOnce,
    }

    #[derive(Debug)]
    struct FaultyBlobstore {
        inner: Arc<dyn BlobstoreEnumerableWithUnlink>,
        fault: UnlinkFault,
        unlink_attempts: Mutex<HashMap<String, usize>>,
    }

    impl FaultyBlobstore {
        fn record_unlink_attempt(&self, key: &str) -> usize {
            let mut unlink_attempts = self.unlink_attempts.lock().expect("lock poisoned");
            let attempts = unlink_attempts.entry(key.to_string()).or_default();
            *attempts += 1;
            *attempts
        }

        fn max_unlink_attempts(&self) -> Option<usize> {
            self.unlink_attempts
                .lock()
                .expect("lock poisoned")
                .values()
                .copied()
                .max()
        }
    }

    impl fmt::Display for FaultyBlobstore {
        fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
            write!(fmt, "FaultyBlobstore<{}>", self.inner)
        }
    }

    #[async_trait]
    impl Blobstore for FaultyBlobstore {
        async fn get<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: &'a str,
        ) -> Result<Option<BlobstoreGetData>> {
            self.inner.get(ctx, key).await
        }

        async fn put_explicit<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: String,
            value: BlobstoreBytes,
            put_behaviour: PutBehaviour,
        ) -> Result<OverwriteStatus> {
            self.inner
                .put_explicit(ctx, key, value, put_behaviour)
                .await
        }

        async fn put_with_status<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: String,
            value: BlobstoreBytes,
        ) -> Result<OverwriteStatus> {
            self.inner.put_with_status(ctx, key, value).await
        }

        async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
            let attempt = self.record_unlink_attempt(key);
            match self.fault {
                UnlinkFault::KeyVanishesAfterListing => self.inner.unlink(ctx, key).await,
                UnlinkFault::UnlinkFails => Err(anyhow!("Injected unlink failure for {key}")),
                UnlinkFault::UnlinkFailsOnce if attempt == 1 => {
                    Err(anyhow!("Injected first unlink failure for {key}"))
                }
                UnlinkFault::UnlinkFailsOnce => self.inner.unlink(ctx, key).await,
            }
        }
    }

    #[async_trait]
    impl BlobstoreKeySource for FaultyBlobstore {
        async fn enumerate<'a>(
            &'a self,
            ctx: &'a CoreContext,
            range: &'a BlobstoreKeyParam,
        ) -> Result<BlobstoreEnumerationData> {
            let enumeration = self.inner.enumerate(ctx, range).await?;
            if let UnlinkFault::KeyVanishesAfterListing = self.fault
                && let Some(key) = enumeration.keys.iter().next()
            {
                self.inner.unlink(ctx, key).await?;
            }
            Ok(enumeration)
        }
    }

    async fn bubble_with_keys_in_faulty_store(
        fb: FacebookInit,
        fault: UnlinkFault,
    ) -> Result<(
        CoreContext,
        Arc<dyn BlobstoreEnumerableWithUnlink>,
        RepoEphemeralStore,
        BubbleId,
        Arc<FaultyBlobstore>,
    )> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, blobstore, repo_blobstore, _) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let faulty = Arc::new(FaultyBlobstore {
            inner: blobstore.clone(),
            fault,
            unlink_attempts: Mutex::new(HashMap::new()),
        });
        let eph = RepoEphemeralStoreBuilder::with_sqlite_in_memory()?.build(
            REPO_ZERO,
            faulty.clone(),
            initial,
            grace,
            BubbleDeletionMode::MarkAndDelete,
        );
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_repo = bubble.wrap_repo_blobstore(repo_blobstore);
        for i in 1..=3 {
            bubble_repo
                .put(
                    &ctx,
                    format!("test_key_{i}"),
                    BlobstoreBytes::from_bytes(format!("test data {i}")),
                )
                .await?;
        }
        Ok((ctx, blobstore, eph, bubble.bubble_id(), faulty))
    }

    #[mononoke::fbinit_test]
    async fn delete_bubble_with_key_vanished_after_listing_test(fb: FacebookInit) -> Result<()> {
        let (ctx, blobstore, eph, bubble_id, _) =
            bubble_with_keys_in_faulty_store(fb, UnlinkFault::KeyVanishesAfterListing).await?;

        let deleted = eph.delete_bubble(&ctx, bubble_id).await?;

        assert_eq!(
            deleted,
            DeleteBubbleOutcome {
                keys_deleted: 2,
                keys_already_deleted: 1,
            },
            "The key that vanished after listing should be counted as already deleted"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert!(
            enumerated.keys.is_empty(),
            "No keys should remain after deleting the bubble"
        );
        let err = eph
            .open_bubble(&ctx, bubble_id)
            .await
            .expect_err("Bubble should no longer exist after deletion");
        assert!(
            matches!(
                err.downcast_ref::<EphemeralBlobstoreError>(),
                Some(EphemeralBlobstoreError::NoSuchBubble(_))
            ),
            "Reopening a deleted bubble should fail with NoSuchBubble"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubble_with_failing_unlink_test(fb: FacebookInit) -> Result<()> {
        let (ctx, blobstore, eph, bubble_id, faulty) =
            bubble_with_keys_in_faulty_store(fb, UnlinkFault::UnlinkFails).await?;

        let result = eph.delete_bubble(&ctx, bubble_id).await;

        assert!(
            result.is_err(),
            "Unlink failures for keys that are still present should fail the deletion"
        );
        assert_eq!(
            faulty.max_unlink_attempts(),
            Some(1),
            "Without a retry policy every key should be unlinked at most once"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert_eq!(
            enumerated.keys.len(),
            3,
            "All keys should remain when unlinking them fails"
        );
        Ok(())
    }

    fn retrying_unlink_policy(max_attempts: usize) -> UnlinkPolicy {
        UnlinkPolicy {
            max_unlinks_per_sec: None,
            retry: Some(UnlinkRetry {
                max_attempts: NonZeroUsize::new(max_attempts)
                    .expect("max_attempts should be positive"),
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
            }),
        }
    }

    #[mononoke::fbinit_test]
    async fn delete_bubbles_retries_failed_unlinks_test(fb: FacebookInit) -> Result<()> {
        let (ctx, blobstore, eph, bubble_id, faulty) =
            bubble_with_keys_in_faulty_store(fb, UnlinkFault::UnlinkFailsOnce).await?;

        let results = eph
            .delete_bubbles(&ctx, vec![bubble_id], 1, retrying_unlink_policy(3))
            .collect::<Vec<_>>()
            .await;

        let [(_, Ok(outcome))] = results.as_slice() else {
            panic!(
                "The bubble should be deleted once its failed unlinks are retried, got {results:?}"
            );
        };
        assert_eq!(
            *outcome,
            DeleteBubbleOutcome {
                keys_deleted: 3,
                keys_already_deleted: 0,
            },
            "Every key should be unlinked by its retry"
        );
        assert_eq!(
            faulty.max_unlink_attempts(),
            Some(2),
            "Each key should succeed on its second attempt"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert!(
            enumerated.keys.is_empty(),
            "No keys should remain after the retries succeed"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubbles_without_retry_fails_on_transient_unlink_failure_test(
        fb: FacebookInit,
    ) -> Result<()> {
        let (ctx, blobstore, eph, bubble_id, faulty) =
            bubble_with_keys_in_faulty_store(fb, UnlinkFault::UnlinkFailsOnce).await?;

        let results = eph
            .delete_bubbles(&ctx, vec![bubble_id], 1, UnlinkPolicy::default())
            .collect::<Vec<_>>()
            .await;

        assert!(
            matches!(results.as_slice(), [(_, Err(_))]),
            "Without a retry policy a single unlink failure should fail the deletion, got {results:?}"
        );
        assert_eq!(
            faulty.max_unlink_attempts(),
            Some(1),
            "Without a retry policy no key should be unlinked twice"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert!(
            !enumerated.keys.is_empty(),
            "Keys whose unlink failed should remain"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubbles_gives_up_after_max_unlink_attempts_test(
        fb: FacebookInit,
    ) -> Result<()> {
        let (ctx, blobstore, eph, bubble_id, faulty) =
            bubble_with_keys_in_faulty_store(fb, UnlinkFault::UnlinkFails).await?;

        let results = eph
            .delete_bubbles(&ctx, vec![bubble_id], 1, retrying_unlink_policy(3))
            .collect::<Vec<_>>()
            .await;

        assert!(
            matches!(results.as_slice(), [(_, Err(_))]),
            "The deletion should fail once a key runs out of unlink attempts, got {results:?}"
        );
        assert_eq!(
            faulty.max_unlink_attempts(),
            Some(3),
            "No key should be unlinked more than the 3 allowed attempts"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert_eq!(
            enumerated.keys.len(),
            3,
            "All keys should remain when every unlink attempt fails"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubbles_respects_unlink_rate_limit_test(fb: FacebookInit) -> Result<()> {
        let initial = Duration::from_hours(30 * 24);
        let grace = Duration::from_hours(6);
        let (ctx, blobstore, repo_blobstore, eph) =
            bootstrap(fb, initial, grace, BubbleDeletionMode::MarkAndDelete)?;
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_repo = bubble.wrap_repo_blobstore(repo_blobstore);
        for i in 1..=6 {
            bubble_repo
                .put(
                    &ctx,
                    format!("test_key_{i}"),
                    BlobstoreBytes::from_bytes(format!("test data {i}")),
                )
                .await?;
        }
        let policy = UnlinkPolicy {
            max_unlinks_per_sec: NonZeroU32::new(20),
            retry: None,
        };

        let started = Instant::now();
        let results = eph
            .delete_bubbles(&ctx, vec![bubble.bubble_id()], 1, policy)
            .collect::<Vec<_>>()
            .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(results.as_slice(), [(_, Ok(outcome))] if outcome.keys_deleted == 6),
            "All 6 keys should be deleted under the rate limit, got {results:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(150),
            "At 20 unlinks/s with a burst of 2, the 4 unlinks after the burst need at least 200ms, took {elapsed:?}"
        );
        let enumerated = blobstore
            .enumerate(&ctx, &BlobstoreKeyParam::from(..))
            .await?;
        assert!(enumerated.keys.is_empty(), "No bubble data should remain");
        Ok(())
    }
}
