/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod dag_items;
mod errors;
mod repair;
mod underived;

use std::collections::HashMap;
use std::iter::Iterator;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Args;
use clientinfo::ClientInfo;
use context::CoreContext;
pub use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::DerivedDataManager;
use ephemeral_blobstore::Bubble;
use ephemeral_blobstore::BubbleId;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use serde::Deserialize;
use serde::Serialize;

#[derive(Args, Debug, Clone)]
pub struct DerivationQueueArgs {
    /// Namespace for the derivation queue
    #[clap(long, default_value = "/mononoke_derivation")]
    pub derivation_queue_namespace: String,

    /// Use the pipeline-specific Zelos config instead of the default
    #[clap(long, default_value_t = false)]
    pub use_pipeline_zelos_config: bool,
}

pub use crate::dag_items::DagItemDep;
pub use crate::dag_items::DagItemId;
pub use crate::dag_items::DagItemInfo;
pub use crate::dag_items::DerivationDagItem;
pub use crate::dag_items::derivation_priority_to_str;
pub use crate::errors::InternalError;
pub use crate::repair::RepairOptions;
pub use crate::repair::RepairResult;
pub use crate::underived::build_underived_batched_graph;

/// Status of a single dependency: suffix and whether its `needed` node exists.
pub struct DepStatus {
    pub suffix: String,
    pub needed_exists: bool,
}

/// Whether a queue item is in the ready state, and which priority queue.
pub enum ReadyState {
    NotReady,
    ReadyHighPri,
    ReadyLowPri,
}

/// Result of inspecting a specific DAG item in the derivation queue.
pub struct InspectResult {
    /// `DagItemInfo` from the freshest source available: ready (high-pri),
    /// then ready (low-pri), then needed. `None` only if the item is not
    /// present in any of those znodes (i.e. not in the queue).
    pub info: Option<DagItemInfo>,
    /// Whether the `needed` znode itself is present, independent of where
    /// `info` was sourced from. The item is considered to be in the queue
    /// iff this is `true`.
    pub needed_exists: bool,
    pub ready_state: ReadyState,
    pub is_deriving: bool,
    pub forward_deps: Vec<DepStatus>,
    pub reverse_deps: Vec<DepStatus>,
}

/// Lightweight item returned from dequeue — just the ID and priority.
/// The full DerivationDagItem is constructed during claim_derivation
/// after fetching data from Zeus.
#[derive(Clone, Debug)]
pub struct DequeuedItem {
    pub dag_item_id: DagItemId,
    pub priority: DerivationPriority,
}

/// Plan produced by `prepare_ack` that classifies reverse dependencies
/// into fast-path (single-dep, can start immediately) vs normal-path
/// (multi-dep, need standard evict processing).
pub struct AckPlan {
    /// Single-dep rdeps where the acked item is the only blocker.
    /// These can start deriving immediately without going through
    /// the dequeue path.
    pub fast_path_items: Vec<DerivationDagItem>,
    /// Multi-dep rdep suffixes that need normal evict processing.
    pub normal_path_rdeps: Vec<String>,
}

/// Counts of znodes deleted by `unsafe_nuke`, keyed by DAG node type name.
#[derive(Debug, Default)]
pub struct NukeStats {
    pub deleted_per_type: HashMap<String, u64>,
}

#[facet::facet]
pub struct RepoDerivationQueues {
    configs_to_queues: HashMap<String, Arc<dyn DerivationQueue + Send + Sync>>,
}

impl RepoDerivationQueues {
    pub fn new(configs_to_queues: HashMap<String, Arc<dyn DerivationQueue + Send + Sync>>) -> Self {
        Self { configs_to_queues }
    }
    pub fn queue(&self, config_name: &str) -> Option<Arc<dyn DerivationQueue + Send + Sync>> {
        self.configs_to_queues.get(config_name).cloned()
    }
    pub fn queues(&self) -> impl Iterator<Item = Arc<dyn DerivationQueue + Send + Sync>> + '_ {
        self.configs_to_queues.values().cloned()
    }
}

#[async_trait]
pub trait DerivationQueue {
    fn for_bubble(&self, bubble: Bubble) -> Arc<dyn DerivationQueue + Send + Sync>;

    async fn enqueue(
        &self,
        ctx: &CoreContext,
        item: DerivationDagItem,
    ) -> Result<EnqueueResponse, InternalError>;

    async fn dequeue(
        &self,
        ctx: &CoreContext,
        limit: usize,
    ) -> Result<DequeueResponse, InternalError>;

    /// Atomically claim an item for derivation by creating a deriving node.
    /// Returns Ok(Some(dag_item)) if claimed successfully with the full item
    /// data fetched from Zeus, Ok(None) if another worker already claimed it
    /// or the item was evicted due to exceeding retry count.
    async fn claim_derivation(
        &self,
        ctx: &CoreContext,
        item: &DequeuedItem,
    ) -> Result<Option<DerivationDagItem>, InternalError>;

    async fn ack(&self, ctx: &CoreContext, item: &DerivationDagItem) -> Result<(), InternalError>;

    /// Scan the item's rdeps and classify into fast-path vs normal-path.
    /// Pure read — no Zelos mutations.
    async fn prepare_ack(
        &self,
        ctx: &CoreContext,
        item: &DerivationDagItem,
    ) -> Result<AckPlan, InternalError>;

    /// Execute the ack plan: process normal-path rdeps, then fire the combined
    /// atomic multi-op that cleans up the acked item and transitions fast-path
    /// items to ready+deriving.
    async fn execute_ack(
        &self,
        ctx: &CoreContext,
        item: &DerivationDagItem,
        plan: AckPlan,
    ) -> Result<(), InternalError>;

    async fn nack(&self, ctx: &CoreContext, item: DerivationDagItem) -> Result<(), InternalError>;

    async fn unsafe_evict(
        &self,
        ctx: &CoreContext,
        item_id: DagItemId,
    ) -> Result<(), InternalError>;

    /// Delete every znode under this queue's `(repo_id, config)` for all DAG
    /// node types. Intended for unwedging a stuck queue — caller should pause
    /// the derivation service first. Aborts on the first Zeus error.
    async fn unsafe_nuke(&self, ctx: &CoreContext) -> Result<NukeStats, InternalError>;

    /// Derive and remove isolated stranded items. Destructive admin operations
    /// must not overlap this operation. Dependency edges are never repaired.
    async fn repair(
        &self,
        ctx: &CoreContext,
        options: RepairOptions,
    ) -> Result<Vec<RepairResult>, InternalError>;

    async fn extend_deriving_ttl(
        &self,
        ctx: &CoreContext,
        item_id: &DagItemId,
    ) -> Result<(), InternalError>;

    async fn watch_existing(
        &self,
        ctx: &CoreContext,
        item_id: DagItemId,
    ) -> Result<EnqueueResponse, InternalError>;

    async fn summary(&self, ctx: &CoreContext) -> Result<DerivationQueueSummary, InternalError>;

    /// Inspect the Zelos DAG state of a specific item, including its node
    /// states, forward dependencies, and reverse dependencies.
    async fn inspect(
        &self,
        ctx: &CoreContext,
        item_id: DagItemId,
    ) -> Result<InspectResult, InternalError>;

    fn derived_data_manager(&self) -> &DerivedDataManager;

    fn repo_id(&self) -> RepositoryId;
}

pub struct EnqueueResponse {
    pub(crate) watch: BoxFuture<'static, anyhow::Result<bool>>,
}

impl EnqueueResponse {
    pub fn new(watch: BoxFuture<'static, anyhow::Result<bool>>) -> Self {
        Self { watch }
    }

    pub async fn is_derived(self) -> anyhow::Result<bool> {
        self.watch.await
    }
}

impl std::fmt::Debug for EnqueueResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Wrapper for Zeus watch")
    }
}

pub enum DequeueResponse {
    Empty {
        ready_queue_watch:
            Box<dyn futures::future::Future<Output = anyhow::Result<()>> + Unpin + Send + Sync>,
    },
    Items(BoxStream<'static, Result<DequeuedItem, InternalError>>),
}

pub struct DerivationQueueSummary<'a> {
    pub queue_size: usize,
    pub high_priority_ready_size: usize,
    pub low_priority_ready_size: usize,
    pub items: BoxStream<'a, Result<DerivationQueueSummaryItem, InternalError>>,
}

#[derive(Serialize, Deserialize)]
pub struct DerivationQueueSummaryItem {
    dag_item_id: DagItemId,
    dag_item_info: DagItemInfo,
    is_ready: bool,
    ready_timestamp: Option<Timestamp>,
    deriving_timestamp: Option<Timestamp>,
}

impl DerivationQueueSummaryItem {
    pub fn new(
        dag_item_id: DagItemId,
        dag_item_info: DagItemInfo,
        is_ready: bool,
        ready_timestamp: Option<Timestamp>,
        deriving_timestamp: Option<Timestamp>,
    ) -> Self {
        Self {
            dag_item_id,
            dag_item_info,
            is_ready,
            ready_timestamp,
            deriving_timestamp,
        }
    }

    pub fn derived_data_type(&self) -> DerivableType {
        self.dag_item_id.derived_data_type
    }

    pub fn enqueue_timestamp(&self) -> Option<Timestamp> {
        self.dag_item_info.enqueue_timestamp()
    }

    pub fn retry_count(&self) -> u64 {
        self.dag_item_info.retry_count()
    }

    pub fn head_cs_id(&self) -> ChangesetId {
        self.dag_item_info.head_cs_id()
    }

    pub fn root_cs_id(&self) -> ChangesetId {
        self.dag_item_id.root_cs_id()
    }

    pub fn bubble_id(&self) -> Option<BubbleId> {
        self.dag_item_info.bubble_id()
    }

    pub fn client_info(&self) -> Option<&ClientInfo> {
        self.dag_item_info.client_info()
    }

    pub fn is_ready(&self) -> bool {
        self.is_ready
    }

    pub fn deriving_timestamp(&self) -> Option<Timestamp> {
        self.deriving_timestamp
    }

    pub fn ready_timestamp(&self) -> Option<Timestamp> {
        self.ready_timestamp
    }

    pub fn priority(&self) -> DerivationPriority {
        self.dag_item_info.priority()
    }
}
