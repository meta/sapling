/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use bulk_derivation::BulkDerivation;
use cacheblob::MemWritesBlobstore;
use clap::ArgGroup;
use clap::Args;
use commit_graph::CommitGraphArc;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivedDataManager;
use derived_data_manager::PipelineMode;
use derived_data_manager::StageId;
use derived_data_manager::StageInputs;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream::BoxStream;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::derive_hg_augmented_manifest::cached_acl_overlay_map;
use mercurial_derivation::derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai_changeset;
use mercurial_derivation::derive_hg_augmented_manifest::normalize_acl_root;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::sharded_augmented_manifest::HgAugmentedManifestEntry;
use mercurial_types::sharded_augmented_manifest::HgAugmentedManifestEnvelope;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use mononoke_types::typed_hash::AclManifestId;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use restricted_paths_common::ArcRestrictedPathsConfigBased;
use restricted_paths_common::NoopRestrictedPathsManifestIdStore;
use restricted_paths_common::RestrictedPathsConfigBased;
use tracing::info;

use super::Repo;
use super::verify_aug_upload::OverwritingOverlay;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifyMismatch {
    pub(crate) cs_id: ChangesetId,
    pub(crate) field: String,
    pub(crate) computed: String,
    pub(crate) expected: String,
}

impl VerifyMismatch {
    fn new(
        cs_id: ChangesetId,
        field: impl Into<String>,
        computed: impl Into<String>,
        expected: impl Into<String>,
    ) -> Self {
        Self {
            cs_id,
            field: field.into(),
            computed: computed.into(),
            expected: expected.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifyOneOutcome {
    Direct,
    FullV2Fallback,
    Mismatch(VerifyMismatch),
}

pub(crate) struct Verifier<'a> {
    ctx: &'a CoreContext,
    repo: &'a Repo,
    manager: &'a DerivedDataManager,
    acl_cache: HashMap<AclManifestId, Arc<HashMap<MPath, AclManifestId>>>,
    restricted_paths: ArcRestrictedPathsConfigBased,
}

impl<'a> Verifier<'a> {
    pub(crate) fn new(
        ctx: &'a CoreContext,
        repo: &'a Repo,
        manager: &'a DerivedDataManager,
    ) -> Self {
        let derivation_ctx = manager.derivation_context(None);
        let real_restricted_paths = derivation_ctx.restricted_paths();
        // Verification must not write restricted-path manifest ids while re-deriving.
        let restricted_paths = Arc::new(RestrictedPathsConfigBased::new(
            real_restricted_paths.config().clone(),
            Arc::new(NoopRestrictedPathsManifestIdStore::new(
                derivation_ctx.repo_id(),
            )),
            None,
        ));

        Self {
            ctx,
            repo,
            manager,
            acl_cache: HashMap::new(),
            restricted_paths,
        }
    }

    fn no_store_manager(&self) -> DerivedDataManager {
        let mem_repo_blobstore = RepoBlobstore::new_with_wrapped_inner_blobstore(
            self.repo.repo_blobstore().clone(),
            |inner| Arc::new(MemWritesBlobstore::new(inner)),
        );
        self.manager
            .with_replaced_blobstore(mem_repo_blobstore)
            .with_replaced_restricted_paths(self.restricted_paths.clone())
    }

    pub(crate) async fn verify_one(&mut self, cs_id: ChangesetId) -> Result<VerifyOneOutcome> {
        let bonsai = cs_id
            .load(self.ctx, self.repo.repo_blobstore())
            .await
            .with_context(|| format!("loading bonsai changeset {cs_id}"))?;

        if bonsai.is_snapshot() {
            bail!("cannot verify snapshot changeset {cs_id}");
        }

        let existing = self
            .fetch_required::<RootHgAugmentedManifestId>(cs_id)
            .await?;

        let acl_root = self.fetch_required::<RootAclManifestId>(cs_id).await?;
        let acl_overlay = normalize_acl_root(&acl_root)
            .with_context(|| format!("normalizing ACL root for {cs_id}"))?;

        let no_store_manager = self.no_store_manager();
        let no_store_derivation_ctx = no_store_manager.derivation_context(None);
        let computed_blobstore = no_store_derivation_ctx.blobstore();

        let acl_map = cached_acl_overlay_map(
            self.ctx,
            computed_blobstore,
            acl_overlay,
            &mut self.acl_cache,
        )
        .await
        .with_context(|| format!("building ACL overlay map for {cs_id}"))?;

        let parent_acl_paths = self
            .parent_acl_paths(computed_blobstore, cs_id, &bonsai)
            .await?;

        let env_stored = existing
            .hg_augmented_manifest_id()
            .load(self.ctx, self.repo.repo_blobstore())
            .await
            .with_context(|| format!("loading stored augmented manifest envelope for {cs_id}"))?;

        // ACL pointer fields are not part of the augmented-manifest content
        // digest. Compare them over the sparse ACL overlay frontier for this
        // changeset and its parents, but intentionally avoid a full augmented
        // tree walk: this verifier is meant to run over large ranges.
        let all_acl_paths = acl_map
            .keys()
            .cloned()
            .chain(parent_acl_paths)
            .collect::<BTreeSet<_>>();

        let stored_root_is_content_derived = env_stored.augmented_manifest.hg_node_id
            == env_stored.augmented_manifest.computed_node_id;
        let (computed, success_outcome) = if stored_root_is_content_derived {
            (
                self.derive_bonsai_direct_root(
                    &no_store_derivation_ctx,
                    computed_blobstore,
                    &bonsai,
                    acl_overlay,
                )
                .await?,
                VerifyOneOutcome::Direct,
            )
        } else {
            (
                self.derive_v2_source_choice_root(&no_store_derivation_ctx, &bonsai)
                    .await
                    .with_context(|| {
                        format!("deriving no-store v2 source-choice manifest for {cs_id}")
                    })?,
                VerifyOneOutcome::FullV2Fallback,
            )
        };

        match self
            .compare_computed_root_to_stored(
                computed_blobstore,
                cs_id,
                computed,
                &env_stored,
                &all_acl_paths,
            )
            .await?
        {
            None => Ok(success_outcome),
            Some(mismatch) => Ok(VerifyOneOutcome::Mismatch(mismatch)),
        }
    }

    async fn verify_pipeline_batch(&mut self, mut cs_ids: Vec<ChangesetId>) -> Result<()> {
        let mut config = self
            .manager
            .repo_config()
            .derived_data_config
            .shadow_pipeline_config
            .clone()
            .context("missing shadow pipeline config")?;
        let types = [
            DerivableType::AclManifests,
            DerivableType::HgAugmentedManifests,
        ];
        config.types = types.into_iter().collect();
        config.stages_by_type.retain(|ty, _| types.contains(ty));
        config.validate()?;

        let computed = RepoBlobstore::new_with_wrapped_inner_blobstore(
            self.repo.repo_blobstore().clone(),
            |inner| Arc::new(OverwritingOverlay(Arc::new(MemWritesBlobstore::new(inner)))),
        );
        let manager = self
            .manager
            .with_replaced_blobstore(computed)
            .with_replaced_restricted_paths(self.restricted_paths.clone())
            .with_pipeline_mode(PipelineMode::Shadow);
        let generations = self
            .repo
            .commit_graph()
            .many_changeset_generations(self.ctx, &cs_ids)
            .await?;
        cs_ids.sort_by_key(|cs_id| generations[cs_id]);
        let batches = derivation_pipeline_driver::plan_batches(
            self.ctx,
            manager.repo_blobstore(),
            &config,
            cs_ids.clone(),
        )
        .await?;
        let order = derivation_pipeline_driver::plan_stages_and_types(&manager, &config, &types)?;
        let mut outputs = Vec::new();
        derivation_pipeline_driver::run_pipeline_batches(
            &manager,
            self.ctx,
            &config,
            &order.sorted_stages,
            &order.sorted_types,
            &batches,
            |batch| outputs.extend_from_slice(batch),
        )
        .await?;

        let canonical = self
            .manager
            .with_stage_inputs(StageInputs::new(&outputs)?)
            .with_pipeline_mode(PipelineMode::Shadow);
        let derivation = manager.derivation_context(None);
        let computed_blobstore = derivation.blobstore();
        for cs_id in cs_ids {
            for ty in types {
                anyhow::ensure!(
                    BulkDerivation::verify_stage_output(
                        &canonical,
                        self.ctx,
                        cs_id,
                        ty,
                        &StageId::Manifest(MPath::ROOT),
                    )
                    .await?,
                    "MISMATCH at {cs_id}: {ty:?} terminal output"
                );
            }
            let stored = self
                .fetch_required::<RootHgAugmentedManifestId>(cs_id)
                .await?;
            let root_id = stored.hg_augmented_manifest_id();
            let expected = root_id.load(self.ctx, self.repo.repo_blobstore()).await?;
            let acl = self.fetch_required::<RootAclManifestId>(cs_id).await?;
            let acl_paths = cached_acl_overlay_map(
                self.ctx,
                self.repo.repo_blobstore(),
                normalize_acl_root(&acl)?,
                &mut self.acl_cache,
            )
            .await?;
            let bonsai = cs_id.load(self.ctx, self.repo.repo_blobstore()).await?;
            let mut paths = self
                .parent_acl_paths(self.repo.repo_blobstore(), cs_id, &bonsai)
                .await?;
            paths.extend(acl_paths.keys().cloned());
            if let Some(mismatch) = self
                .compare_computed_root_to_stored(
                    computed_blobstore,
                    cs_id,
                    root_id,
                    &expected,
                    &paths,
                )
                .await?
            {
                bail!(
                    "MISMATCH at {}: {} computed={} expected={}",
                    mismatch.cs_id,
                    mismatch.field,
                    mismatch.computed,
                    mismatch.expected
                );
            }
            info!("pipeline match: cs_id={cs_id} hg_augmented_manifest_id={root_id}");
        }
        Ok(())
    }

    async fn derive_bonsai_direct_root(
        &self,
        derivation_ctx: &DerivationContext,
        computed_blobstore: &Arc<dyn KeyedBlobstore>,
        bonsai: &BonsaiChangeset,
        acl_overlay: Option<AclManifestId>,
    ) -> Result<HgAugmentedManifestId> {
        let cs_id = bonsai.get_changeset_id();
        let aug_parents = derivation_ctx
            .fetch_parents::<RootHgAugmentedManifestId>(self.ctx, bonsai)
            .await
            .with_context(|| format!("fetching augmented parent roots for {cs_id}"))?
            .into_iter()
            .map(|p| p.hg_augmented_manifest_id())
            .collect();
        let restricted_paths = derivation_ctx.restricted_paths();
        derive_augmented_manifest_from_bonsai_changeset(
            self.ctx,
            computed_blobstore,
            bonsai,
            aug_parents,
            &restricted_paths,
            acl_overlay,
        )
        .await
        .with_context(|| format!("directly deriving Bonsai augmented manifest for {cs_id}"))
    }

    async fn derive_v2_source_choice_root(
        &self,
        derivation_ctx: &DerivationContext,
        bonsai: &BonsaiChangeset,
    ) -> Result<HgAugmentedManifestId> {
        let cs_id = bonsai.get_changeset_id();
        let aug_parents = derivation_ctx
            .fetch_parents::<RootHgAugmentedManifestId>(self.ctx, bonsai)
            .await
            .with_context(|| format!("fetching augmented parent roots for {cs_id}"))?
            .into_iter()
            .map(|p| p.hg_augmented_manifest_id())
            .collect();
        RootHgAugmentedManifestId::derive_from_mapped_hg_with_parents(
            self.ctx,
            derivation_ctx,
            bonsai,
            aug_parents,
        )
        .await?
        .map(|root| root.hg_augmented_manifest_id())
        .ok_or_else(|| anyhow!("mapped Hg manifest not found for {cs_id}"))
    }

    async fn compare_computed_root_to_stored(
        &self,
        computed_blobstore: &Arc<dyn KeyedBlobstore>,
        cs_id: ChangesetId,
        computed: HgAugmentedManifestId,
        env_stored: &HgAugmentedManifestEnvelope,
        all_acl_paths: &BTreeSet<MPath>,
    ) -> Result<Option<VerifyMismatch>> {
        let env_computed = computed
            .load(self.ctx, computed_blobstore)
            .await
            .with_context(|| format!("loading computed augmented manifest envelope for {cs_id}"))?;

        if let Some(mismatch) = compare_envelopes(cs_id, &env_computed, env_stored) {
            return Ok(Some(mismatch));
        }

        compare_acl_pointers(
            self.ctx,
            computed_blobstore,
            self.repo.repo_blobstore(),
            cs_id,
            &env_computed,
            env_stored,
            all_acl_paths.clone(),
        )
        .await
    }

    async fn fetch_required<Derivable>(&self, cs_id: ChangesetId) -> Result<Derivable>
    where
        Derivable: derived_data_manager::BonsaiDerivable,
    {
        self.manager
            .fetch_derived::<Derivable>(self.ctx, cs_id, None)
            .await
            .with_context(|| format!("fetching stored {} for {cs_id}", Derivable::NAME))?
            .ok_or_else(|| anyhow!("{} is not derived for {cs_id}", Derivable::NAME))
    }

    async fn parent_acl_paths<Store>(
        &mut self,
        blobstore: &Store,
        cs_id: ChangesetId,
        bonsai: &mononoke_types::BonsaiChangeset,
    ) -> Result<BTreeSet<MPath>>
    where
        Store: KeyedBlobstore + 'static,
    {
        let mut paths = BTreeSet::new();
        for parent_cs_id in bonsai.parents() {
            let acl_root = self
                .fetch_required::<RootAclManifestId>(parent_cs_id)
                .await
                .with_context(|| format!("fetching parent ACL root {parent_cs_id} for {cs_id}"))?;
            let parent_overlay =
                normalize_acl_root(&acl_root).context("normalizing parent ACL root")?;
            let parent_map =
                cached_acl_overlay_map(self.ctx, blobstore, parent_overlay, &mut self.acl_cache)
                    .await
                    .context("building parent ACL overlay map")?;
            paths.extend(parent_map.keys().cloned());
        }
        Ok(paths)
    }
}

pub(crate) fn compare_envelopes(
    cs_id: ChangesetId,
    computed: &HgAugmentedManifestEnvelope,
    expected: &HgAugmentedManifestEnvelope,
) -> Option<VerifyMismatch> {
    if computed.augmented_manifest_id != expected.augmented_manifest_id {
        return Some(VerifyMismatch::new(
            cs_id,
            "augmented_manifest_id (Blake3)",
            format!("{:?}", computed.augmented_manifest_id),
            format!("{:?}", expected.augmented_manifest_id),
        ));
    }
    if computed.augmented_manifest_size != expected.augmented_manifest_size {
        return Some(VerifyMismatch::new(
            cs_id,
            "augmented_manifest_size",
            computed.augmented_manifest_size.to_string(),
            expected.augmented_manifest_size.to_string(),
        ));
    }
    if computed.augmented_manifest.hg_node_id != expected.augmented_manifest.hg_node_id {
        return Some(VerifyMismatch::new(
            cs_id,
            "hg_node_id",
            computed.augmented_manifest.hg_node_id.to_string(),
            expected.augmented_manifest.hg_node_id.to_string(),
        ));
    }
    if computed.augmented_manifest.computed_node_id != expected.augmented_manifest.computed_node_id
    {
        return Some(VerifyMismatch::new(
            cs_id,
            "computed_node_id",
            computed.augmented_manifest.computed_node_id.to_string(),
            expected.augmented_manifest.computed_node_id.to_string(),
        ));
    }
    if computed.augmented_manifest.p1 != expected.augmented_manifest.p1 {
        return Some(VerifyMismatch::new(
            cs_id,
            "p1",
            format!("{:?}", computed.augmented_manifest.p1),
            format!("{:?}", expected.augmented_manifest.p1),
        ));
    }
    if computed.augmented_manifest.p2 != expected.augmented_manifest.p2 {
        return Some(VerifyMismatch::new(
            cs_id,
            "p2",
            format!("{:?}", computed.augmented_manifest.p2),
            format!("{:?}", expected.augmented_manifest.p2),
        ));
    }
    if computed.augmented_manifest.acl_manifest_directory_id
        != expected.augmented_manifest.acl_manifest_directory_id
    {
        return Some(VerifyMismatch::new(
            cs_id,
            "acl_manifest_directory_id",
            format!(
                "{:?}",
                computed.augmented_manifest.acl_manifest_directory_id
            ),
            format!(
                "{:?}",
                expected.augmented_manifest.acl_manifest_directory_id
            ),
        ));
    }
    None
}

fn entry_kind(entry: &Option<HgAugmentedManifestEntry>) -> &'static str {
    match entry {
        None => "missing",
        Some(HgAugmentedManifestEntry::DirectoryNode(_)) => "directory",
        Some(HgAugmentedManifestEntry::FileNode(_)) => "file",
    }
}

/// Compare ACL pointers for the supplied sparse path set.
///
/// This is intentionally not a full augmented-manifest audit. Callers should
/// pass paths that can carry, or previously carried, ACL overlay pointers;
/// exhaustive recursive ACL-pointer parity is covered by derivation tests on
/// small fixtures, not by the large-range admin verifier.
pub(crate) async fn compare_acl_pointers(
    ctx: &CoreContext,
    computed_blobstore: &impl KeyedBlobstore,
    expected_blobstore: &impl KeyedBlobstore,
    cs_id: ChangesetId,
    computed: &HgAugmentedManifestEnvelope,
    expected: &HgAugmentedManifestEnvelope,
    acl_paths: BTreeSet<MPath>,
) -> Result<Option<VerifyMismatch>> {
    // Root acl_manifest_directory_id is already checked by compare_envelopes.
    // Walk non-root ACL overlay directories component-by-component using raw
    // lookup (ManifestOps conversion drops ACL pointers).
    for acl_path in acl_paths.into_iter().filter(|path| !path.is_root()) {
        if let Some(mismatch) = compare_acl_path_pointers(
            ctx,
            computed_blobstore,
            expected_blobstore,
            cs_id,
            computed,
            expected,
            &acl_path,
        )
        .await?
        {
            return Ok(Some(mismatch));
        }
    }

    Ok(None)
}

async fn compare_acl_path_pointers(
    ctx: &CoreContext,
    computed_blobstore: &impl KeyedBlobstore,
    expected_blobstore: &impl KeyedBlobstore,
    cs_id: ChangesetId,
    computed: &HgAugmentedManifestEnvelope,
    expected: &HgAugmentedManifestEnvelope,
    acl_path: &MPath,
) -> Result<Option<VerifyMismatch>> {
    let components = acl_path.iter().collect::<Vec<_>>();

    let mut cur_computed = computed.clone();
    let mut cur_expected = expected.clone();
    for (depth, elem) in components.iter().enumerate() {
        let entry_computed = cur_computed
            .augmented_manifest
            .lookup(ctx, computed_blobstore, elem)
            .await
            .with_context(|| format!("looking up computed ACL path {acl_path}"))?;
        let entry_expected = cur_expected
            .augmented_manifest
            .lookup(ctx, expected_blobstore, elem)
            .await
            .with_context(|| format!("looking up expected ACL path {acl_path}"))?;

        match (entry_computed, entry_expected) {
            (
                Some(HgAugmentedManifestEntry::DirectoryNode(dir_computed)),
                Some(HgAugmentedManifestEntry::DirectoryNode(dir_expected)),
            ) => {
                let partial_path =
                    MPath::from_elements(components[..=depth].iter().copied()).to_string();

                if dir_computed.acl_manifest_directory_id != dir_expected.acl_manifest_directory_id
                {
                    return Ok(Some(VerifyMismatch::new(
                        cs_id,
                        format!("entry acl_manifest_directory_id at {partial_path}"),
                        format!("{:?}", dir_computed.acl_manifest_directory_id),
                        format!("{:?}", dir_expected.acl_manifest_directory_id),
                    )));
                }

                let child_computed = HgAugmentedManifestId::new(dir_computed.treenode)
                    .load(ctx, computed_blobstore)
                    .await
                    .with_context(|| {
                        format!("loading computed child envelope at {partial_path}")
                    })?;
                let child_expected = HgAugmentedManifestId::new(dir_expected.treenode)
                    .load(ctx, expected_blobstore)
                    .await
                    .with_context(|| {
                        format!("loading expected child envelope at {partial_path}")
                    })?;

                if child_computed.augmented_manifest.acl_manifest_directory_id
                    != child_expected.augmented_manifest.acl_manifest_directory_id
                {
                    return Ok(Some(VerifyMismatch::new(
                        cs_id,
                        format!("envelope acl_manifest_directory_id at {partial_path}"),
                        format!(
                            "{:?}",
                            child_computed.augmented_manifest.acl_manifest_directory_id
                        ),
                        format!(
                            "{:?}",
                            child_expected.augmented_manifest.acl_manifest_directory_id
                        ),
                    )));
                }

                if depth < components.len() - 1 {
                    cur_computed = child_computed;
                    cur_expected = child_expected;
                }
            }
            (None, None)
            | (
                Some(HgAugmentedManifestEntry::FileNode(_)),
                Some(HgAugmentedManifestEntry::FileNode(_)),
            ) => break,
            (entry_c, entry_s) => {
                let partial_path =
                    MPath::from_elements(components[..=depth].iter().copied()).to_string();
                return Ok(Some(VerifyMismatch::new(
                    cs_id,
                    format!("acl entry type at {partial_path}"),
                    entry_kind(&entry_c).to_string(),
                    entry_kind(&entry_s).to_string(),
                )));
            }
        }
    }

    Ok(None)
}

const DEFAULT_VERIFY_AUG_DIRECT_BATCH_SIZE: u64 = 100;
const MAX_VERIFY_AUG_DIRECT_BATCH_SIZE: u64 = 10_000;
const MAX_VERIFY_AUG_DIRECT_CONCURRENCY: u64 = 100;

#[derive(Args)]
#[clap(group(
    ArgGroup::new("changeset_range")
        .required(true)
        .args(&["first", "last"]),
))]
pub(super) struct VerifyAugDirectArgs {
    /// Inclusive upper bound changeset (hex bonsai changeset ID).
    /// Mutually exclusive with --bookmark.
    #[clap(long, conflicts_with = "bookmark")]
    end_id: Option<ChangesetId>,

    /// Resolve upper bound from a bookmark (e.g. master).
    /// Mutually exclusive with --end-id. Defaults to 'master' when neither is set.
    #[clap(long, short = 'B', conflicts_with = "end_id")]
    bookmark: Option<BookmarkKey>,

    /// Optional inclusive lower bound for --first. When omitted, --first starts
    /// at the first-parent root reachable from --end-id/--bookmark.
    #[clap(long, requires = "first")]
    start_id: Option<ChangesetId>,

    /// Number of oldest changesets to validate in the selected ancestor subgraph.
    #[clap(long, value_parser = clap::value_parser!(u64).range(1..))]
    first: Option<u64>,

    /// Number of most recent changesets to validate, ending at --end-id/--bookmark.
    #[clap(long, value_parser = clap::value_parser!(u64).range(1..))]
    last: Option<u64>,

    /// Number of changesets to select per verifier batch. Default: 100. Maximum: 10000.
    #[clap(
        long,
        default_value_t = DEFAULT_VERIFY_AUG_DIRECT_BATCH_SIZE,
        value_parser = clap::value_parser!(u64).range(1..=MAX_VERIFY_AUG_DIRECT_BATCH_SIZE),
    )]
    batch_size: u64,

    /// Replay ACL and augmented manifests using the shadow pipeline layout, with writes kept in memory.
    #[clap(long)]
    pipeline: bool,

    /// Number of verifier batches to process concurrently. Maximum: 100.
    #[clap(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=MAX_VERIFY_AUG_DIRECT_CONCURRENCY))]
    concurrency: u64,
}

async fn resolve_end_id(
    ctx: &CoreContext,
    repo: &Repo,
    end_id: Option<ChangesetId>,
    bookmark: Option<BookmarkKey>,
) -> Result<ChangesetId> {
    match end_id {
        Some(end_id) => Ok(end_id),
        None => {
            let bookmark = match bookmark {
                Some(bookmark) => bookmark,
                None => BookmarkKey::new("master")?,
            };
            repo.bookmarks()
                .get(ctx.clone(), &bookmark, Freshness::MostRecent)
                .await?
                .ok_or_else(|| anyhow!("bookmark '{bookmark}' not found"))
        }
    }
}

struct VerifyBatch {
    index: u64,
    cs_ids: Vec<ChangesetId>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VerifyBatchCounts {
    processed: u64,
    direct: u64,
    full_v2_fallback: u64,
}

fn batch_changeset_stream(
    cs_ids: BoxStream<'static, Result<ChangesetId>>,
    batch_size: u64,
) -> Result<BoxStream<'static, Result<VerifyBatch>>> {
    let batch_size =
        usize::try_from(batch_size).context("--batch-size value does not fit in usize")?;
    Ok(cs_ids
        .try_chunks(batch_size)
        .enumerate()
        .map(|(index, batch)| match batch {
            Ok(cs_ids) => Ok(VerifyBatch {
                index: index as u64 + 1,
                cs_ids,
            }),
            Err(err) => Err(err.1),
        })
        .boxed())
}

async fn select_last_changesets(
    ctx: &CoreContext,
    repo: &Repo,
    end_id: ChangesetId,
    count: u64,
) -> Result<BoxStream<'static, Result<ChangesetId>>> {
    let count = usize::try_from(count).context("--last value does not fit in usize")?;
    Ok(repo
        .commit_graph()
        .ancestors_difference_stream(ctx, vec![end_id], vec![])
        .await?
        .take(count)
        .boxed())
}

async fn select_last_changeset_batches(
    ctx: &CoreContext,
    repo: &Repo,
    end_id: ChangesetId,
    count: u64,
    batch_size: u64,
) -> Result<BoxStream<'static, Result<VerifyBatch>>> {
    let cs_ids = select_last_changesets(ctx, repo, end_id, count).await?;
    batch_changeset_stream(cs_ids, batch_size)
}

async fn first_parent_root(
    ctx: &CoreContext,
    repo: &Repo,
    end_id: ChangesetId,
) -> Result<ChangesetId> {
    repo.commit_graph()
        .p1_linear_graph()
        .skip_tree_level_ancestor(ctx, end_id, 0)
        .await
        .with_context(|| format!("finding first-parent root for {end_id}"))?
        .map(|node| node.cs_id)
        .ok_or_else(|| anyhow!("no first-parent root found for {end_id}"))
}

async fn select_first_changesets(
    ctx: &CoreContext,
    repo: &Repo,
    start_id: ChangesetId,
    end_id: ChangesetId,
    count: u64,
) -> Result<BoxStream<'static, Result<ChangesetId>>> {
    if !repo
        .commit_graph()
        .is_ancestor(ctx, start_id, end_id)
        .await
        .with_context(|| format!("checking whether {start_id} is an ancestor of {end_id}"))?
    {
        bail!("start changeset {start_id} is not an ancestor of end changeset {end_id}");
    }

    let commit_graph = repo.commit_graph_arc();
    let ctx = ctx.clone();
    Ok(async_stream::try_stream! {
        let start_generation = commit_graph
            .changeset_generation(&ctx, start_id)
            .await
            .with_context(|| format!("loading generation for start changeset {start_id}"))?;
        let mut frontier = BTreeMap::new();
        frontier
            .entry(start_generation)
            .or_insert_with(BTreeSet::new)
            .insert(start_id);
        let mut seen = HashSet::from([start_id]);
        let mut selected = 0_u64;

        while selected < count {
            let Some((_generation, cs_ids)) = frontier.pop_first() else {
                break;
            };
            for cs_id in cs_ids {
                yield cs_id;
                selected += 1;
                if selected == count {
                    break;
                }

                let children = commit_graph
                    .changeset_children(&ctx, cs_id)
                    .await
                    .with_context(|| format!("loading children of {cs_id}"))?;
                let children = commit_graph
                    .filter_ancestors(&ctx, end_id, children)
                    .await
                    .with_context(|| {
                        format!("filtering children of {cs_id} to ancestors of {end_id}")
                    })?;
                let generations = commit_graph
                    .many_changeset_generations(&ctx, &children)
                    .await
                    .with_context(|| format!("loading child generations for {cs_id}"))?;
                for child in children {
                    if seen.insert(child) {
                        let generation = generations
                            .get(&child)
                            .copied()
                            .ok_or_else(|| anyhow!("missing generation for child changeset {child}"))?;
                        frontier
                            .entry(generation)
                            .or_insert_with(BTreeSet::new)
                            .insert(child);
                    }
                }
            }
        }
    }
    .boxed())
}

async fn select_first_changeset_batches(
    ctx: &CoreContext,
    repo: &Repo,
    start_id: ChangesetId,
    end_id: ChangesetId,
    count: u64,
    batch_size: u64,
) -> Result<BoxStream<'static, Result<VerifyBatch>>> {
    let cs_ids = select_first_changesets(ctx, repo, start_id, end_id, count).await?;
    batch_changeset_stream(cs_ids, batch_size)
}

async fn verify_aug_direct_batch(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    batch: VerifyBatch,
    pipeline: bool,
) -> Result<(u64, VerifyBatchCounts)> {
    let index = batch.index;
    let mut counts = VerifyBatchCounts::default();
    let mut verifier = Verifier::new(ctx, repo, manager);

    if pipeline {
        counts.processed = batch.cs_ids.len() as u64;
        verifier.verify_pipeline_batch(batch.cs_ids).await?;
        return Ok((index, counts));
    }

    for cs_id in batch.cs_ids {
        counts.processed += 1;
        match verifier.verify_one(cs_id).await? {
            VerifyOneOutcome::Direct => counts.direct += 1,
            VerifyOneOutcome::FullV2Fallback => counts.full_v2_fallback += 1,
            VerifyOneOutcome::Mismatch(m) => {
                return Err(anyhow!(
                    "MISMATCH at {}: {} diverges: computed={} expected={}",
                    m.cs_id,
                    m.field,
                    m.computed,
                    m.expected,
                ));
            }
        }
    }

    Ok((index, counts))
}

pub(super) async fn verify_aug_direct(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    args: VerifyAugDirectArgs,
) -> Result<()> {
    let end_id = resolve_end_id(ctx, repo, args.end_id, args.bookmark).await?;
    let (requested_count, batches) = match (args.first, args.last) {
        (Some(count), None) => {
            let start_id = match args.start_id {
                Some(start_id) => start_id,
                None => first_parent_root(ctx, repo, end_id).await?,
            };
            let batches =
                select_first_changeset_batches(ctx, repo, start_id, end_id, count, args.batch_size)
                    .await?;
            println!("start-id={start_id}");
            (count, batches)
        }
        (None, Some(count)) => (
            count,
            select_last_changeset_batches(ctx, repo, end_id, count, args.batch_size).await?,
        ),
        _ => unreachable!("clap requires exactly one of --first or --last"),
    };

    info!("verifying up to {} changesets", requested_count);

    let concurrency =
        usize::try_from(args.concurrency).context("--concurrency value does not fit in usize")?;
    let mut processed = 0;
    let mut direct = 0;
    let mut full_v2_fallback = 0;

    batches
        .map_ok(|batch| verify_aug_direct_batch(ctx, repo, manager, batch, args.pipeline))
        .try_buffer_unordered(concurrency)
        .try_for_each(|(batch_index, counts)| {
            processed += counts.processed;
            direct += counts.direct;
            full_v2_fallback += counts.full_v2_fallback;
            info!(
                "progress: batch={batch_index} size={} processed={processed} direct={direct} full-v2-fallback={full_v2_fallback}",
                counts.processed,
            );
            future::ready(Ok(()))
        })
        .await?;

    println!("done: processed={processed} direct={direct} full-v2-fallback={full_v2_fallback}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    use bonsai_git_mapping::BonsaiGitMapping;
    use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
    use bookmarks::Bookmarks;
    use cacheblob::MemWritesKeyedBlobstore;
    use changesets_creation::save_changesets;
    use clap::Parser;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use derived_data_manager::BonsaiDerivable;
    use fbinit::FacebookInit;
    use filenodes::Filenodes;
    use filestore::FilestoreConfig;
    use futures::TryStreamExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::override_just_knobs;
    use mercurial_derivation::MappedHgChangesetId;
    use mercurial_types::HgNodeHash;
    use mercurial_types::HgParents;
    use mercurial_types::blobs::ChangesetMetadata;
    use mercurial_types::blobs::HgBlobChangeset;
    use mercurial_types::blobs::HgChangesetContent;
    use mercurial_types::blobs::UploadHgNodeHash;
    use mercurial_types::blobs::UploadHgTreeEntry;
    use mercurial_types::fetch_raw_manifest_bytes;
    use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
    use mercurial_types::sharded_augmented_manifest::ShardedHgAugmentedManifest;
    use mononoke_macros::mononoke;
    use mononoke_types::DateTime;
    use mononoke_types::FileType;
    use mononoke_types::NonRootMPath;
    use mononoke_types::RepoPath;
    use mononoke_types::hash::Blake2;
    use mononoke_types::hash::Blake3;
    use mononoke_types::hash::Sha1 as ContentSha1;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;
    use mononoke_types::sharded_map_v2::ShardedMapV2Node;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreArc;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

    use super::*;

    const SLACL_PROJECT1: &[u8] =
        b"repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n";

    #[facet::container]
    struct TestRepo {
        #[delegate(
            dyn BonsaiHgMapping,
            dyn BonsaiGitMapping,
            dyn BonsaiGlobalrevMapping,
            dyn BonsaiSvnrevMapping,
            RepoBlobstore,
            dyn Bookmarks,
            CommitGraph,
            dyn Filenodes,
            FilestoreConfig,
            RepoDerivedData,
            RepoIdentity,
        )]
        repo: Repo,

        #[facet]
        commit_graph_writer: dyn CommitGraphWriter,
    }

    fn knob_overrides(extra: impl IntoIterator<Item = (&'static str, bool)>) -> JustKnobsInMemory {
        let mut knobs = HashMap::new();
        knobs.extend(
            extra
                .into_iter()
                .map(|(name, value)| (name.to_string(), KnobVal::Bool(value))),
        );
        JustKnobsInMemory::new(knobs)
    }

    async fn derive_stored_aug(
        ctx: &CoreContext,
        repo: &Repo,
        cs_ids: &[ChangesetId],
    ) -> Result<()> {
        let manager = repo.repo_derived_data().manager();
        fixtures::store_hg_changesets_as_uploaded(ctx, repo, cs_ids.to_vec()).await?;
        manager
            .derive_exactly_batch::<RootAclManifestId>(ctx, cs_ids.to_vec(), None)
            .await?;
        manager
            .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, cs_ids.to_vec(), None)
            .await?;
        Ok(())
    }

    struct SuppliedRootFixture {
        repo: TestRepo,
        root: ChangesetId,
        stored_aug: RootHgAugmentedManifestId,
    }

    async fn create_supplied_root_fixture(ctx: &CoreContext) -> Result<SuppliedRootFixture> {
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;

        let stored_aug = {
            let manager = repo.repo.repo_derived_data().manager();
            fixtures::store_hg_changesets_as_uploaded(ctx, &repo, vec![root]).await?;
            manager
                .derive_exactly_batch::<RootAclManifestId>(ctx, vec![root], None)
                .await?;

            let mapped_root = manager
                .fetch_derived::<MappedHgChangesetId>(ctx, root, None)
                .await?
                .ok_or_else(|| anyhow!("missing MappedHgChangesetId for {root}"))?;
            let original_hg_changeset = mapped_root
                .hg_changeset_id()
                .load(ctx, repo.repo.repo_blobstore())
                .await?;
            let original_manifest_id = original_hg_changeset.manifestid();
            let raw_manifest =
                fetch_raw_manifest_bytes(ctx, repo.repo.repo_blobstore(), original_manifest_id)
                    .await?;

            let supplied_manifest_hash = HgNodeHash::new(NodeSha1::from_byte_array([0xAB; 20]));
            let upload = UploadHgTreeEntry {
                upload_node_id: UploadHgNodeHash::Supplied(supplied_manifest_hash),
                contents: raw_manifest.into_inner(),
                p1: None,
                p2: None,
                path: RepoPath::root(),
                computed_node_id: None,
            };
            let (supplied_manifest_id, upload_fut) =
                upload.upload(ctx.clone(), repo.repo.repo_blobstore_arc())?;
            upload_fut.await?;

            repo.repo
                .repo_blobstore()
                .unlink(ctx, &mapped_root.hg_changeset_id().blobstore_key())
                .await?;
            let supplied_hg_changeset = HgBlobChangeset::new_with_id(
                mapped_root.hg_changeset_id(),
                HgChangesetContent::new_from_parts(
                    HgParents::new(None, None),
                    supplied_manifest_id,
                    ChangesetMetadata {
                        user: "test <test@example.com>".into(),
                        time: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
                        extra: BTreeMap::new(),
                        message: "supplied root manifest".into(),
                    },
                    vec![NonRootMPath::new("file.txt")?],
                ),
            );
            supplied_hg_changeset
                .save(ctx, repo.repo.repo_blobstore())
                .await?;
            manager
                .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, vec![root], None)
                .await?;
            manager
                .fetch_derived::<RootHgAugmentedManifestId>(ctx, root, None)
                .await?
                .ok_or_else(|| anyhow!("missing RootHgAugmentedManifestId for {root}"))?
        };

        Ok(SuppliedRootFixture {
            repo,
            root,
            stored_aug,
        })
    }

    struct OverwrittenRootMappingFixture {
        repo: TestRepo,
        second: ChangesetId,
        root_aug: RootHgAugmentedManifestId,
        correct_second_aug: RootHgAugmentedManifestId,
    }

    async fn create_overwritten_root_mapping_fixture(
        ctx: &CoreContext,
    ) -> Result<OverwrittenRootMappingFixture> {
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;
        let second = CreateCommitContext::new(ctx, &repo, vec![root])
            .add_file("file.txt", b"two")
            .commit()
            .await?;
        derive_stored_aug(ctx, &repo.repo, &[root, second]).await?;

        let (root_aug, correct_second_aug) = {
            let manager = repo.repo.repo_derived_data().manager();
            let derivation_ctx = manager.derivation_context(None);
            let root_aug = manager
                .fetch_derived::<RootHgAugmentedManifestId>(ctx, root, None)
                .await?
                .ok_or_else(|| anyhow!("missing RootHgAugmentedManifestId for {root}"))?;
            let correct_second_aug = manager
                .fetch_derived::<RootHgAugmentedManifestId>(ctx, second, None)
                .await?
                .ok_or_else(|| anyhow!("missing RootHgAugmentedManifestId for {second}"))?;
            assert_ne!(
                root_aug.hg_augmented_manifest_id(),
                correct_second_aug.hg_augmented_manifest_id()
            );
            let second_mapping_key = format!(
                "derived_root_hgaugmentedmanifest.{}{}",
                derivation_ctx.mapping_key_prefix::<RootHgAugmentedManifestId>(),
                second
            );
            derivation_ctx
                .blobstore()
                .unlink(ctx, &second_mapping_key)
                .await?;
            root_aug
                .clone()
                .store_mapping(ctx, &derivation_ctx, second)
                .await?;
            let overwritten_second_aug = manager
                .fetch_derived::<RootHgAugmentedManifestId>(ctx, second, None)
                .await?
                .ok_or_else(|| {
                    anyhow!("missing overwritten RootHgAugmentedManifestId for {second}")
                })?;
            assert_eq!(
                overwritten_second_aug.hg_augmented_manifest_id(),
                root_aug.hg_augmented_manifest_id()
            );
            (root_aug, correct_second_aug)
        };

        Ok(OverwrittenRootMappingFixture {
            repo,
            second,
            root_aug,
            correct_second_aug,
        })
    }

    fn test_changeset_id() -> ChangesetId {
        ChangesetId::new(Blake2::from_byte_array([0x99; 32]))
    }

    fn test_acl_id(byte: u8) -> AclManifestId {
        AclManifestId::new(Blake2::from_byte_array([byte; 32]))
    }

    fn test_envelope(byte: u8, acl_id: Option<AclManifestId>) -> HgAugmentedManifestEnvelope {
        let node_hash = HgNodeHash::new(NodeSha1::from_byte_array([byte; 20]));
        HgAugmentedManifestEnvelope {
            augmented_manifest_id: Blake3::from_byte_array([byte; 32]),
            augmented_manifest_size: byte as u64,
            augmented_manifest: ShardedHgAugmentedManifest {
                hg_node_id: node_hash,
                p1: None,
                p2: None,
                computed_node_id: node_hash,
                subentries: ShardedMapV2Node::default(),
                acl_manifest_directory_id: acl_id,
            },
        }
    }

    fn test_file_entry(byte: u8) -> HgAugmentedManifestEntry {
        HgAugmentedManifestEntry::FileNode(HgAugmentedFileLeafNode {
            file_type: FileType::Regular,
            filenode: HgNodeHash::new(NodeSha1::from_byte_array([byte; 20])),
            content_blake3: Blake3::from_byte_array([byte; 32]),
            content_sha1: ContentSha1::from_byte_array([byte; 20]),
            total_size: byte as u64,
            file_header_metadata: None,
        })
    }

    #[derive(Parser)]
    struct VerifyAugDirectArgsParser {
        #[clap(flatten)]
        _args: VerifyAugDirectArgs,
    }

    #[mononoke::test]
    fn verify_aug_direct_args_rejects_excessive_concurrency() {
        // Given a concurrency value above the operational safety cap.
        let excessive_concurrency = (MAX_VERIFY_AUG_DIRECT_CONCURRENCY + 1).to_string();

        // When parsing the verifier arguments.
        let error = match VerifyAugDirectArgsParser::try_parse_from([
            "verify-aug-direct",
            "--last",
            "1",
            "--concurrency",
            excessive_concurrency.as_str(),
        ]) {
            Ok(_) => panic!("concurrency above the cap should be rejected"),
            Err(error) => error,
        };

        // Then clap rejects the value before any verification work can fan out.
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(error.to_string().contains(&format!(
            "is not in 1..={MAX_VERIFY_AUG_DIRECT_CONCURRENCY}"
        )));
    }

    #[mononoke::fbinit_test]
    async fn with_replaced_restricted_paths_preserves_noop_store_for_verifier(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a derived-data manager and a verifier-style restricted-paths
        // config with the same config but a no-op manifest-id store.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let manager = repo.repo.repo_derived_data().manager();
        let original_ctx = manager.derivation_context(None);
        let original_restricted_paths = original_ctx.restricted_paths();
        let noop_restricted_paths = std::sync::Arc::new(RestrictedPathsConfigBased::new(
            original_restricted_paths.config().clone(),
            std::sync::Arc::new(NoopRestrictedPathsManifestIdStore::new(
                original_ctx.repo_id(),
            )),
            None,
        ));

        // When replacing restricted paths on a cloned manager.
        let replaced_manager =
            manager.with_replaced_restricted_paths(noop_restricted_paths.clone());

        // Then the cloned manager uses the verifier's no-op restricted paths,
        // while the original manager remains unchanged.
        let replaced_restricted_paths =
            replaced_manager.derivation_context(None).restricted_paths();
        assert!(std::sync::Arc::ptr_eq(
            &replaced_restricted_paths,
            &noop_restricted_paths,
        ));
        let unchanged_restricted_paths = manager.derivation_context(None).restricted_paths();
        assert!(std::sync::Arc::ptr_eq(
            &unchanged_restricted_paths,
            &original_restricted_paths,
        ));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn select_last_changeset_batches_groups_selected_changesets_into_bounded_batches(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a linear repository history with five changesets.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;
        let second = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("file.txt", b"two")
            .commit()
            .await?;
        let third = CreateCommitContext::new(&ctx, &repo, vec![second])
            .add_file("file.txt", b"three")
            .commit()
            .await?;
        let fourth = CreateCommitContext::new(&ctx, &repo, vec![third])
            .add_file("file.txt", b"four")
            .commit()
            .await?;
        let fifth = CreateCommitContext::new(&ctx, &repo, vec![fourth])
            .add_file("file.txt", b"five")
            .commit()
            .await?;

        // When selecting the last five changesets in batches of two.
        let batches = select_last_changeset_batches(&ctx, &repo.repo, fifth, 5, 2)
            .await?
            .map_ok(|batch| batch.cs_ids)
            .try_collect::<Vec<_>>()
            .await?;

        // Then the selected changesets are emitted in bounded batches.
        assert_eq!(
            batches,
            vec![vec![fifth, fourth], vec![third, second], vec![root]]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn select_first_changeset_batches_starts_from_discovered_first_parent_root(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a linear repository history with five changesets.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;
        let second = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("file.txt", b"two")
            .commit()
            .await?;
        let third = CreateCommitContext::new(&ctx, &repo, vec![second])
            .add_file("file.txt", b"three")
            .commit()
            .await?;
        let fourth = CreateCommitContext::new(&ctx, &repo, vec![third])
            .add_file("file.txt", b"four")
            .commit()
            .await?;
        let fifth = CreateCommitContext::new(&ctx, &repo, vec![fourth])
            .add_file("file.txt", b"five")
            .commit()
            .await?;

        // When selecting the first five changesets from the discovered first-parent root in batches of two.
        let start_id = first_parent_root(&ctx, &repo.repo, fifth).await?;
        let batches = select_first_changeset_batches(&ctx, &repo.repo, start_id, fifth, 5, 2)
            .await?
            .map_ok(|batch| batch.cs_ids)
            .try_collect::<Vec<_>>()
            .await?;

        // Then selection starts from the root and emits progress-sized oldest-to-newest batches.
        assert_eq!(
            batches,
            vec![vec![root, second], vec![third, fourth], vec![fifth]]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn select_first_changeset_batches_uses_start_id_override(fb: FacebookInit) -> Result<()> {
        // Given a linear repository history with four changesets.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;
        let second = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("file.txt", b"two")
            .commit()
            .await?;
        let third = CreateCommitContext::new(&ctx, &repo, vec![second])
            .add_file("file.txt", b"three")
            .commit()
            .await?;
        let fourth = CreateCommitContext::new(&ctx, &repo, vec![third])
            .add_file("file.txt", b"four")
            .commit()
            .await?;

        // When selecting from an explicit resume start id in batches of two.
        let batches = select_first_changeset_batches(&ctx, &repo.repo, second, fourth, 3, 2)
            .await?
            .map_ok(|batch| batch.cs_ids)
            .try_collect::<Vec<_>>()
            .await?;

        // Then selection starts from the override instead of rediscovering the root.
        assert_eq!(batches, vec![vec![second, third], vec![fourth]]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn compare_envelopes_detects_mismatch(_fb: FacebookInit) -> Result<()> {
        let cs_id = test_changeset_id();
        let expected = test_envelope(1, None);
        let matching = expected.clone();
        let different = test_envelope(2, None);

        assert_eq!(compare_envelopes(cs_id, &matching, &expected), None);
        let mismatch = compare_envelopes(cs_id, &different, &expected)
            .ok_or_else(|| anyhow!("expected envelope mismatch"))?;
        assert_eq!(mismatch.field, "augmented_manifest_id (Blake3)");

        let acl_a = test_envelope(1, Some(test_acl_id(1)));
        let acl_b = test_envelope(1, Some(test_acl_id(2)));
        let mismatch = compare_envelopes(cs_id, &acl_a, &acl_b)
            .ok_or_else(|| anyhow!("expected ACL mismatch"))?;
        assert_eq!(mismatch.field, "acl_manifest_directory_id");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_one_validates_acl_pointer_changeset(fb: FacebookInit) -> Result<()> {
        // Given a changeset with an ACL-bearing directory and stored old-path derived data.
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/file.txt", b"content")
            .add_file("dir/.slacl", SLACL_PROJECT1)
            .commit()
            .await?;
        derive_stored_aug(&ctx, &repo.repo, &[root]).await?;
        let manager = repo.repo.repo_derived_data().manager();
        let acl_root = manager
            .fetch_derived::<RootAclManifestId>(&ctx, root, None)
            .await?
            .ok_or_else(|| anyhow!("missing RootAclManifestId for {root}"))?;
        let acl_overlay = normalize_acl_root(&acl_root)?;
        let mem = MemWritesKeyedBlobstore::new(repo.repo.repo_blobstore().clone());
        let acl_map = cached_acl_overlay_map(&ctx, &mem, acl_overlay, &mut HashMap::new()).await?;
        assert!(!acl_map.is_empty());

        // When verifying the changeset.
        let mut verifier = Verifier::new(&ctx, &repo.repo, manager);
        let outcome = verifier.verify_one(root).await?;

        // Then direct derivation matches the stored data, including ACL pointers.
        assert_eq!(outcome, VerifyOneOutcome::Direct);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn compare_acl_pointers_accepts_matching_files(fb: FacebookInit) -> Result<()> {
        // Given computed and stored manifests that both resolve a selected ACL path to a file.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let subentries = ShardedMapV2Node::from_entries(
            &ctx,
            repo.repo.repo_blobstore(),
            [(b"dir".as_slice(), test_file_entry(1))],
        )
        .await?;
        let mut computed = test_envelope(1, None);
        computed.augmented_manifest.subentries = subentries.clone();
        let mut expected = test_envelope(1, None);
        expected.augmented_manifest.subentries = subentries;

        // When comparing ACL pointers for that path.
        let result = compare_acl_pointers(
            &ctx,
            repo.repo.repo_blobstore(),
            repo.repo.repo_blobstore(),
            test_changeset_id(),
            &computed,
            &expected,
            BTreeSet::from([MPath::new("dir")?]),
        )
        .await?;

        // Then matching file/file entries do not cause an ACL mismatch.
        assert_eq!(result, None);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_one_rejects_snapshot_changesets(fb: FacebookInit) -> Result<()> {
        // Given a snapshot changeset selected for augmented-manifest validation.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let mut bonsai = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", b"content")
            .create_commit_object()
            .await?;
        bonsai.is_snapshot = true;
        let bonsai = bonsai.freeze()?;
        let cs_id = bonsai.get_changeset_id();
        save_changesets(&ctx, &repo, vec![bonsai]).await?;
        let manager = repo.repo.repo_derived_data().manager();
        let mut verifier = Verifier::new(&ctx, &repo.repo, manager);

        // When validating the snapshot changeset.
        let error = verifier
            .verify_one(cs_id)
            .await
            .expect_err("snapshot changesets should abort validation");

        // Then validation aborts with the selected changeset id instead of skipping it.
        assert!(
            format!("{error:#}").contains(&format!("cannot verify snapshot changeset {cs_id}"))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_one_end_to_end(fb: FacebookInit) -> Result<()> {
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;
        let second = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("file.txt", b"two")
            .commit()
            .await?;
        let third = CreateCommitContext::new(&ctx, &repo, vec![second])
            .add_file("other.txt", b"three")
            .commit()
            .await?;
        let cs_ids = vec![root, second, third];
        derive_stored_aug(&ctx, &repo.repo, &cs_ids).await?;

        let manager = repo.repo.repo_derived_data().manager();
        let mut verifier = Verifier::new(&ctx, &repo.repo, manager);
        for cs_id in cs_ids {
            assert_eq!(verifier.verify_one(cs_id).await?, VerifyOneOutcome::Direct);
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_one_forces_bonsai_direct_for_root_even_when_hg_mapping_exists(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a root changeset with stored old-path augmented data and an Hg
        // mapping whose Hg changeset blob is no longer loadable.
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", b"one")
            .commit()
            .await?;
        derive_stored_aug(&ctx, &repo.repo, &[root]).await?;

        let manager = repo.repo.repo_derived_data().manager();
        let mapped_root = manager
            .fetch_derived::<MappedHgChangesetId>(&ctx, root, None)
            .await?
            .ok_or_else(|| anyhow!("missing MappedHgChangesetId for {root}"))?;
        let hg_changeset_key = mapped_root.hg_changeset_id().blobstore_key();
        repo.repo
            .repo_blobstore()
            .unlink(&ctx, &hg_changeset_key)
            .await?;
        assert!(
            repo.repo
                .repo_blobstore()
                .get(&ctx, &hg_changeset_key)
                .await?
                .is_none(),
            "fixture should remove the mapped Hg changeset blob",
        );

        // When verifying the changeset.
        let mut verifier = Verifier::new(&ctx, &repo.repo, manager);
        let outcome = verifier.verify_one(root).await?;

        // Then verification succeeds without using the mapped Hg changeset as
        // the computed-side input.
        assert_eq!(outcome, VerifyOneOutcome::Direct);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_one_accepts_v2_source_choice_when_bonsai_direct_mismatches_stored(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a mapped changeset whose canonical Hg manifest has a supplied
        // root id that differs from the content-derived Bonsai root.
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let fixture = create_supplied_root_fixture(&ctx).await?;
        let manager = fixture.repo.repo.repo_derived_data().manager();
        let stored_env = fixture
            .stored_aug
            .hg_augmented_manifest_id()
            .load(&ctx, fixture.repo.repo.repo_blobstore())
            .await?;

        // When verifying the changeset through the forced Bonsai-direct verifier.
        let mut verifier = Verifier::new(&ctx, &fixture.repo.repo, manager);
        let outcome = verifier.verify_one(fixture.root).await?;

        // Then verification succeeds through the full-v2 fallback because
        // no-store v2 source-choice matches the stored mapped-Hg data.
        assert_eq!(outcome, VerifyOneOutcome::FullV2Fallback);
        let stored_after_verify = fixture
            .stored_aug
            .hg_augmented_manifest_id()
            .load(&ctx, fixture.repo.repo.repo_blobstore())
            .await?;
        assert_eq!(stored_after_verify, stored_env);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_aug_direct_batch_counts_full_v2_fallback_as_success(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a mapped changeset whose stored augmented root follows the
        // mapped-Hg source-choice path rather than pure Bonsai-direct output.
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let fixture = create_supplied_root_fixture(&ctx).await?;
        let manager = fixture.repo.repo.repo_derived_data().manager();

        // When verifying a batch containing that changeset.
        let (batch_index, counts) = verify_aug_direct_batch(
            &ctx,
            &fixture.repo.repo,
            manager,
            VerifyBatch {
                index: 42,
                cs_ids: vec![fixture.root],
            },
            false,
        )
        .await?;

        // Then the batch succeeds while surfacing that the changeset matched
        // only through the full-v2 source-choice fallback.
        assert_eq!(batch_index, 42);
        assert_eq!(
            counts,
            VerifyBatchCounts {
                processed: 1,
                direct: 0,
                full_v2_fallback: 1,
            }
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verifier_v2_source_choice_ignores_current_root_mapping(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a linear repository with stored old-path augmented data, but
        // the second commit's augmented-root mapping points to the first
        // commit's valid augmented root.
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let fixture = create_overwritten_root_mapping_fixture(&ctx).await?;
        let second = fixture.second;
        let manager = fixture.repo.repo.repo_derived_data().manager();
        let verifier = Verifier::new(&ctx, &fixture.repo.repo, manager);
        let no_store_manager = verifier.no_store_manager();
        let no_store_derivation_ctx = no_store_manager.derivation_context(None);
        let second_bonsai = second
            .load(&ctx, fixture.repo.repo.repo_blobstore())
            .await
            .with_context(|| format!("loading second bonsai changeset {second}"))?;

        // When computing the second commit's v2 source-choice root with
        // shared parent roots in the no-store derivation context.
        let computed_second = verifier
            .derive_v2_source_choice_root(&no_store_derivation_ctx, &second_bonsai)
            .await?;

        // Then the computation is independent of the current stored root
        // mapping and does not repair that mapping in persistent storage.
        assert_eq!(
            computed_second,
            fixture.correct_second_aug.hg_augmented_manifest_id()
        );
        assert_ne!(computed_second, fixture.root_aug.hg_augmented_manifest_id());
        let stored_second_aug_after_compute = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, second, None)
            .await?
            .ok_or_else(|| anyhow!("missing RootHgAugmentedManifestId for {second}"))?;
        assert_eq!(
            stored_second_aug_after_compute.hg_augmented_manifest_id(),
            fixture.root_aug.hg_augmented_manifest_id()
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_one_reports_mismatch_instead_of_using_current_root_mapping_as_computed_result(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a linear repository with stored old-path derived data for both
        // commits, but the second commit's augmented-root mapping points to
        // the first commit's valid augmented root.
        override_just_knobs(knob_overrides([]));
        let ctx = CoreContext::test_mock(fb);
        let fixture = create_overwritten_root_mapping_fixture(&ctx).await?;
        let second = fixture.second;
        let manager = fixture.repo.repo.repo_derived_data().manager();

        // When verifying the changeset whose stored root mapping was replaced.
        let mut verifier = Verifier::new(&ctx, &fixture.repo.repo, manager);
        let outcome = verifier.verify_one(second).await?;

        // Then the verifier reports the stored-root corruption instead of
        // accepting the stored root as its computed result.
        let mismatch = match outcome {
            VerifyOneOutcome::Mismatch(mismatch) => mismatch,
            outcome => {
                return Err(anyhow!(
                    "expected verifier to report overwritten root mapping for {second}, got {outcome:?}"
                ));
            }
        };
        assert_eq!(mismatch.cs_id, second);
        assert_eq!(mismatch.field, "augmented_manifest_id (Blake3)");
        assert_ne!(mismatch.computed, mismatch.expected);
        Ok(())
    }
}
