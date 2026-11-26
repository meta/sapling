/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use bytes::Bytes;
use cloned::cloned;
use context::CoreContext;
use derived_data::batch::DEFAULT_STACK_FILE_CHANGES_LIMIT;
use derived_data::batch::FileConflicts;
use derived_data::batch::SplitOptions;
use derived_data::batch::split_bonsais_in_linear_stacks;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivableType;
use derived_data_manager::DerivationContext;
use derived_data_manager::dependencies;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use stats::prelude::*;
use tracing::debug;

use crate::access_labels;
use crate::derive_hg_from_augmented;

/// How many commits of a stack store their Hg trees at once, from augmented
/// manifests.
const STACK_CONCURRENCY: usize = 10;

define_stats! {
    prefix = "mononoke.derived_data.hgchangesets";
    new_parallel: timeseries(Rate, Sum),
}

use derived_data_service_if as thrift;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct MappedHgChangesetId(HgChangesetId);

impl MappedHgChangesetId {
    pub(crate) fn new(hg_changeset_id: HgChangesetId) -> Self {
        MappedHgChangesetId(hg_changeset_id)
    }

    pub fn hg_changeset_id(&self) -> HgChangesetId {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct HgChangesetDeriveOptions {
    pub set_committer_field: bool,
}

#[async_trait]
impl BonsaiDerivable for MappedHgChangesetId {
    const VARIANT: DerivableType = DerivableType::HgChangesets;

    type Dependencies = dependencies![RootHgAugmentedManifestId];

    async fn derive_single(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsai: BonsaiChangeset,
        parents: Vec<Self>,
        known: Option<&HashMap<ChangesetId, Self>>,
    ) -> Result<Self> {
        if bonsai.is_snapshot() {
            bail!("Can't derive Hg changeset for snapshot")
        }
        let subtree_change_sources = get_subtree_change_sources(
            ctx,
            derivation_ctx,
            &bonsai,
            known.unwrap_or(&HashMap::new()),
        )
        .await?;
        let derivation_opts = get_hg_changeset_derivation_options(derivation_ctx);
        if derive_hg_from_augmented::derives_from_augmented(derivation_ctx)? {
            return derive_hg_from_augmented::derive_from_augmented(
                ctx,
                derivation_ctx,
                bonsai,
                parents,
                subtree_change_sources,
                &derivation_opts,
            )
            .await;
        }
        let (derived, _) = crate::derive_hg_changeset::derive_from_parents(
            ctx,
            derivation_ctx.blobstore(),
            bonsai,
            parents,
            subtree_change_sources,
            &derivation_opts,
            derivation_ctx.restricted_paths(),
        )
        .await?;
        Ok(derived)
    }

    async fn derive_batch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
    ) -> Result<HashMap<ChangesetId, Self>> {
        let ctx = &ctx.with_access_label(access_labels::HG_CHANGESET_DERIVATION);
        if bonsais.is_empty() {
            return Ok(HashMap::new());
        }
        if bonsais.iter().any(|bonsai| bonsai.is_snapshot()) {
            bail!("Can't derive Hg changeset for snapshot");
        }
        let from_augmented = derive_hg_from_augmented::derives_from_augmented(derivation_ctx)?;

        STATS::new_parallel.add_value(1);
        let linear_stacks = split_bonsais_in_linear_stacks(
            &bonsais,
            SplitOptions {
                file_conflicts: FileConflicts::ChangeDelete,
                copy_info: true,
                file_changes_limit: DEFAULT_STACK_FILE_CHANGES_LIMIT,
            },
        )?;
        let mut res: HashMap<ChangesetId, Self> = HashMap::new();
        let batch_len = bonsais.len();

        let derivation_opts = get_hg_changeset_derivation_options(derivation_ctx);

        let mut bonsais = bonsais;
        for stack in linear_stacks {
            let derived_parents = future::try_join_all(
                stack
                    .parents
                    .into_iter()
                    .map(|p| derivation_ctx.fetch_unknown_dependency::<Self>(ctx, Some(&res), p)),
            )
            .await?;
            if let Some(item) = stack.stack_items.first() {
                debug!(
                    "derive hgchangeset batch at {} (stack of {} from batch of {})",
                    item.cs_id.to_hex(),
                    stack.stack_items.len(),
                    batch_len,
                );
            }

            // after the line below `bonsais` will contain all the bonsais that we are
            // going to derive now, and `left_bonsais` will contain all the bonsais that
            // we are going to derive in the next step
            let left_bonsais = bonsais.split_off(stack.stack_items.len());
            if from_augmented {
                // Only hashing a changeset needs its parents' Hg changesets,
                // so the rest runs for many commits of the stack at once.
                let mut prepared = stream::iter(bonsais)
                    .map(|bonsai| {
                        cloned!(ctx, derivation_ctx);
                        // Spawned, so that it goes on while the loop below
                        // generates earlier changesets.
                        let prepare = mononoke::spawn_task(async move {
                            let root = derivation_ctx
                                .fetch_dependency::<RootHgAugmentedManifestId>(
                                    &ctx,
                                    bonsai.get_changeset_id(),
                                )
                                .await?
                                .hg_augmented_manifest_id();
                            let (parent_roots, files) =
                                derive_hg_from_augmented::store_trees_and_list_files(
                                    &ctx,
                                    &derivation_ctx,
                                    &bonsai,
                                    root,
                                )
                                .await?;
                            anyhow::Ok((bonsai, root, parent_roots, files))
                        });
                        async move { prepare.await? }
                    })
                    .buffered(STACK_CONCURRENCY);
                while let Some((bonsai, root, parent_roots, files)) = prepared.try_next().await? {
                    let parents = derivation_ctx
                        .fetch_unknown_parents(ctx, Some(&res), &bonsai)
                        .await?;
                    let subtree_change_sources =
                        get_subtree_change_sources(ctx, derivation_ctx, &bonsai, &res).await?;
                    let cs_id = bonsai.get_changeset_id();
                    let derived = derive_hg_from_augmented::generate_from_augmented(
                        ctx,
                        derivation_ctx,
                        bonsai,
                        root,
                        parents,
                        parent_roots,
                        files,
                        subtree_change_sources,
                        &derivation_opts,
                    )
                    .await?;
                    res.insert(cs_id, derived);
                }
            } else if derived_parents.len() > 1 || bonsais.len() == 1 {
                // From Hg manifests, we can't derive stack for a merge commit
                // or for a commit that contains renames, or subtree changes so
                // let's derive it without batching
                for bonsai in bonsais {
                    let parents = derivation_ctx
                        .fetch_unknown_parents(ctx, Some(&res), &bonsai)
                        .await?;
                    let cs_id = bonsai.get_changeset_id();
                    let derived =
                        Self::derive_single(ctx, derivation_ctx, bonsai, parents, Some(&res))
                            .await?;
                    res.insert(cs_id, derived);
                }
            } else {
                let first = stack.stack_items.first().map(|item| item.cs_id);
                let last = stack.stack_items.last().map(|item| item.cs_id);
                let derived =
                    crate::derive_hg_changeset::derive_simple_hg_changeset_stack_without_copy_info(
                        ctx,
                        derivation_ctx.blobstore(),
                        bonsais,
                        derived_parents.first().cloned(),
                        &derivation_opts,
                        derivation_ctx.restricted_paths(),
                    )
                    .await
                    .with_context(|| format!("failed deriving stack of {first:?} to {last:?}"))?;

                // This pattern is used to convert a ref to tuple into a tuple of refs.
                #[allow(clippy::map_identity)]
                res.extend(derived.into_iter().map(|(csid, hg_cs_id)| (csid, hg_cs_id)));
            }
            bonsais = left_bonsais;
        }

        // Flushed here so that batched writes reach storage under this
        // derivation's label rather than the manager's context.
        derivation_ctx.flush(ctx).await?;
        Ok(res)
    }

    async fn store_mapping(
        self,
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Result<()> {
        derivation_ctx
            .bonsai_hg_mapping()?
            .add(
                ctx,
                BonsaiHgMappingEntry {
                    hg_cs_id: self.0,
                    bcs_id: changeset_id,
                },
            )
            .await?;
        Ok(())
    }

    async fn store_mapping_batch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        derived: Vec<(ChangesetId, Self)>,
    ) -> Result<()> {
        let entries: Vec<_> = derived
            .into_iter()
            .map(|(bcs_id, hg_cs_id)| BonsaiHgMappingEntry {
                hg_cs_id: hg_cs_id.0,
                bcs_id,
            })
            .collect();
        derivation_ctx
            .bonsai_hg_mapping()?
            .bulk_add(ctx, &entries)
            .await?;
        Ok(())
    }

    async fn fetch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Result<Option<Self>> {
        Ok(Self::fetch_batch(ctx, derivation_ctx, &[changeset_id])
            .await?
            .into_iter()
            .next()
            .map(|(_, hg_id)| hg_id))
    }

    async fn fetch_batch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_ids: &[ChangesetId],
    ) -> Result<HashMap<ChangesetId, Self>> {
        Ok(derivation_ctx
            .bonsai_hg_mapping()?
            .get(ctx, changeset_ids.to_vec().into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, MappedHgChangesetId(entry.hg_cs_id)))
            .collect())
    }

    fn from_thrift(data: thrift::DerivedData) -> Result<Self> {
        if let thrift::DerivedData::hg_changeset(
            thrift::DerivedDataHgChangeset::mapped_hgchangeset_id(id),
        ) = data
        {
            HgChangesetId::from_thrift(id).map(Self)
        } else {
            Err(anyhow!(
                "Can't convert {} from provided thrift::DerivedData",
                Self::NAME,
            ))
        }
    }

    fn into_thrift(data: Self) -> Result<thrift::DerivedData> {
        Ok(thrift::DerivedData::hg_changeset(
            thrift::DerivedDataHgChangeset::mapped_hgchangeset_id(data.0.into_thrift()),
        ))
    }
}

/// Derive the Hg changeset of `bonsai` from the augmented manifest `root`, as
/// `scm/mononoke:hgmanifest_retirement_stage` 1 derives it, storing its Hg
/// trees and Hg changeset but not its mapping. The parents' Hg changesets
/// must be derived. This lets a check compare the result with the stored Hg
/// changeset, through a blobstore that keeps writes in memory.
pub async fn derive_hg_changeset_from_augmented_root(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    root: HgAugmentedManifestId,
) -> Result<HgChangesetId> {
    let parents = derivation_ctx
        .fetch_parents::<MappedHgChangesetId>(ctx, &bonsai)
        .await?;
    let subtree_change_sources =
        get_subtree_change_sources(ctx, derivation_ctx, &bonsai, &HashMap::new()).await?;
    let derivation_opts = get_hg_changeset_derivation_options(derivation_ctx);
    Ok(derive_hg_from_augmented::derive_from_augmented_root(
        ctx,
        derivation_ctx,
        bonsai,
        root,
        parents,
        subtree_change_sources,
        &derivation_opts,
    )
    .await?
    .hg_changeset_id())
}

fn get_hg_changeset_derivation_options(
    derivation_ctx: &DerivationContext,
) -> HgChangesetDeriveOptions {
    HgChangesetDeriveOptions {
        set_committer_field: derivation_ctx.config().hg_set_committer_extra,
    }
}

pub(crate) async fn get_subtree_change_sources(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    mapping: &HashMap<ChangesetId, MappedHgChangesetId>,
) -> Result<HashMap<ChangesetId, HgChangesetId>> {
    let subtree_change_sources = bonsai
        .subtree_changes()
        .iter()
        .flat_map(|(_path, change)| change.change_source().map(|(csid, _)| csid))
        .collect::<HashSet<_>>();
    let mut sources = HashMap::new();
    let mut other = Vec::new();
    for source in subtree_change_sources {
        if let Some(hg_cs_id) = mapping.get(&source) {
            sources.insert(source, hg_cs_id.hg_changeset_id());
        } else {
            other.push(source);
        }
    }
    if !other.is_empty() {
        sources.extend(
            derivation_ctx
                .bonsai_hg_mapping()?
                .get(ctx, other.into())
                .await?
                .into_iter()
                .map(|entry| (entry.bcs_id, entry.hg_cs_id)),
        )
    };
    Ok(sources)
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RootHgAugmentedManifestId(HgAugmentedManifestId);

impl RootHgAugmentedManifestId {
    pub fn new(hg_augmented_manifest_id: HgAugmentedManifestId) -> Self {
        RootHgAugmentedManifestId(hg_augmented_manifest_id)
    }

    pub fn hg_augmented_manifest_id(&self) -> HgAugmentedManifestId {
        self.0
    }

    /// Map `changeset_id` to this root when its envelopes were built outside
    /// derivation, e.g. at tree upload. Records the root's restricted
    /// directories first, as derivation would have. The augmented manifest
    /// depends on the ACL manifest, so the caller maps `RootAclManifestId`
    /// before calling this.
    pub async fn store_prebuilt_mapping(
        self,
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        hg_cs_id: HgChangesetId,
        changeset_id: ChangesetId,
    ) -> Result<()> {
        crate::derive_hg_augmented_manifest::track_all_restricted_paths(
            ctx,
            derivation_ctx.restricted_paths(),
            hg_cs_id,
            self.0,
            Arc::clone(derivation_ctx.blobstore()),
        )
        .await?;
        self.store_mapping(ctx, derivation_ctx, changeset_id).await
    }

    /// Derives the mapped-Hg source choice from caller-provided parent content.
    /// This does not fetch or store completion for those parents.
    pub async fn derive_from_mapped_hg_with_parents(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsai: &BonsaiChangeset,
        aug_parents: Vec<HgAugmentedManifestId>,
    ) -> Result<Option<Self>> {
        let Some(hg_manifest_id) = crate::augmented_derivation::lookup_mapped_root_hg_manifest_id(
            ctx,
            derivation_ctx,
            bonsai.get_changeset_id(),
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(Self(
            crate::augmented_derivation::derive_from_mapped_hg_manifest(
                ctx,
                derivation_ctx,
                bonsai,
                hg_manifest_id,
                aug_parents,
            )
            .await?,
        )))
    }
}

impl TryFrom<BlobstoreBytes> for RootHgAugmentedManifestId {
    type Error = anyhow::Error;

    fn try_from(blob: BlobstoreBytes) -> Result<Self> {
        HgAugmentedManifestId::from_bytes(&blob.into_bytes()).map(Self)
    }
}

impl TryFrom<BlobstoreGetData> for RootHgAugmentedManifestId {
    type Error = anyhow::Error;

    fn try_from(blob_get_data: BlobstoreGetData) -> Result<Self> {
        blob_get_data.into_bytes().try_into()
    }
}

impl From<RootHgAugmentedManifestId> for BlobstoreBytes {
    fn from(root_hg_augmented_manifest_id: RootHgAugmentedManifestId) -> Self {
        BlobstoreBytes::from_bytes(Bytes::copy_from_slice(
            root_hg_augmented_manifest_id.0.as_bytes(),
        ))
    }
}

pub fn format_key(derivation_ctx: &DerivationContext, cs_id: ChangesetId) -> String {
    let root_prefix = "derived_root_hgaugmentedmanifest.";
    let key_prefix = derivation_ctx.mapping_key_prefix::<RootHgAugmentedManifestId>();
    format!("{root_prefix}{key_prefix}{cs_id}")
}

pub(crate) fn hg_augmented_manifest_id_from_derived_data(
    data: thrift::DerivedData,
    type_name: &str,
) -> Result<HgAugmentedManifestId> {
    if let thrift::DerivedData::hg_augmented_manifest(
        thrift::DerivedDataHgAugmentedManifest::root_hg_augmented_manifest_id(id),
    ) = data
    {
        HgAugmentedManifestId::from_thrift(id)
    } else {
        Err(anyhow!(
            "Can't convert {type_name} from provided thrift::DerivedData",
        ))
    }
}

pub(crate) fn hg_augmented_manifest_id_into_derived_data(
    id: HgAugmentedManifestId,
) -> thrift::DerivedData {
    thrift::DerivedData::hg_augmented_manifest(
        thrift::DerivedDataHgAugmentedManifest::root_hg_augmented_manifest_id(id.into_thrift()),
    )
}

#[async_trait]
impl BonsaiDerivable for RootHgAugmentedManifestId {
    const VARIANT: DerivableType = DerivableType::HgAugmentedManifests;

    type Dependencies = dependencies![RootAclManifestId];

    async fn derive_single(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsai: BonsaiChangeset,
        parents: Vec<Self>,
        _known: Option<&HashMap<ChangesetId, Self>>,
    ) -> Result<Self> {
        crate::augmented_derivation::derive_single_root(ctx, derivation_ctx, bonsai, parents).await
    }

    async fn derive_batch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
    ) -> Result<HashMap<ChangesetId, Self>> {
        crate::augmented_derivation::derive_batch_roots(ctx, derivation_ctx, bonsais).await
    }

    async fn store_mapping(
        self,
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Result<()> {
        let key = format_key(derivation_ctx, changeset_id);
        derivation_ctx
            .blobstore()
            .put(ctx, key, self.into())
            .await?;
        Ok(())
    }

    async fn fetch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Result<Option<Self>> {
        let key = format_key(derivation_ctx, changeset_id);
        Ok(derivation_ctx
            .blobstore()
            .get(ctx, &key)
            .await?
            .map(TryInto::try_into)
            .transpose()?)
    }

    fn from_thrift(data: thrift::DerivedData) -> Result<Self> {
        hg_augmented_manifest_id_from_derived_data(data, Self::NAME).map(Self)
    }

    fn into_thrift(data: Self) -> Result<thrift::DerivedData> {
        Ok(hg_augmented_manifest_id_into_derived_data(data.0))
    }
}

#[cfg(test)]
mod test {
    use blobstore::KeyedBlobstore;
    use blobstore::Loadable;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::BookmarkKey;
    use bookmarks::Bookmarks;
    use borrowed::borrowed;
    use cloned::cloned;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphRef;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use fixtures::BranchEven;
    use fixtures::BranchUneven;
    use fixtures::BranchWide;
    use fixtures::Linear;
    use fixtures::ManyDiamonds;
    use fixtures::ManyFilesDirs;
    use fixtures::MergeEven;
    use fixtures::MergeUneven;
    use fixtures::TestRepoFixture;
    use fixtures::UnsharedMergeEven;
    use fixtures::UnsharedMergeUneven;
    use futures::Future;
    use futures::TryStreamExt;
    use mercurial_types::HgAugmentedManifestEnvelope;
    use mercurial_types::HgManifestId;
    use mononoke_macros::mononoke;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::DeriveHgChangeset;

    #[derive(Clone)]
    #[facet::container]
    struct TestRepo {
        #[facet]
        bonsai_hg_mapping: dyn BonsaiHgMapping,
        #[facet]
        bookmarks: dyn Bookmarks,
        #[facet]
        repo_blobstore: RepoBlobstore,
        #[facet]
        repo_derived_data: RepoDerivedData,
        #[facet]
        filestore_config: FilestoreConfig,
        #[facet]
        commit_graph: CommitGraph,
        #[facet]
        commit_graph_writer: dyn CommitGraphWriter,
        #[facet]
        repo_identity: RepoIdentity,
    }

    mod direct_augmented_manifest;

    async fn all_commits_descendants_to_ancestors(
        ctx: CoreContext,
        repo: TestRepo,
    ) -> Result<Vec<(ChangesetId, HgChangesetId)>> {
        let master_book = BookmarkKey::new("master").unwrap();
        let bcs_id = repo
            .bookmarks
            .get(ctx.clone(), &master_book, bookmarks::Freshness::MostRecent)
            .await?
            .ok_or_else(|| anyhow!("Missing master bookmark"))?;

        repo.commit_graph()
            .ancestors_difference_stream(&ctx, vec![bcs_id], vec![])
            .await?
            .and_then(move |new_bcs_id| {
                cloned!(ctx, repo);
                async move {
                    let hg_cs_id = repo.derive_hg_changeset(&ctx, new_bcs_id).await?;
                    Result::<_>::Ok((new_bcs_id, hg_cs_id))
                }
            })
            .try_collect()
            .await
    }

    async fn assert_hgmanifest_blob_absent(
        ctx: &CoreContext,
        repo: &TestRepo,
        manifest_id: HgManifestId,
        cs_id: ChangesetId,
    ) -> Result<()> {
        let key = manifest_id.blobstore_key();
        assert!(
            repo.repo_blobstore().get(ctx, &key).await?.is_none(),
            "HgManifest blob {key} should not be persisted for {cs_id}",
        );
        Ok(())
    }

    async fn verify_repo<F, Fut>(fb: FacebookInit, repo_func: F) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = TestRepo>,
    {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = repo_func().await;
        println!("Processing {}", repo.repo_identity.name());
        borrowed!(ctx, repo);

        let commits_desc_to_anc =
            all_commits_descendants_to_ancestors(ctx.clone(), repo.clone()).await?;

        // Recreate repo from scratch and derive everything again
        let repo = repo_func().await;
        let csids = commits_desc_to_anc
            .clone()
            .into_iter()
            .rev()
            .map(|(cs_id, _)| cs_id)
            .collect::<Vec<_>>();
        let manager = repo.repo_derived_data().manager();

        manager
            .derive_exactly_batch::<RootAclManifestId>(ctx, csids.clone(), None)
            .await?;
        manager
            .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, csids.clone(), None)
            .await?;
        manager
            .derive_exactly_batch::<MappedHgChangesetId>(ctx, csids.clone(), None)
            .await?;
        let batch_derived = manager
            .fetch_derived_batch::<MappedHgChangesetId>(ctx, csids, None)
            .await?;

        for (cs_id, hg_cs_id) in commits_desc_to_anc.into_iter().rev() {
            println!("{} {} {:?}", cs_id, hg_cs_id, batch_derived.get(&cs_id));
            assert_eq!(batch_derived.get(&cs_id).map(|x| x.0), Some(hg_cs_id));
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifest_derive_returns_existing_root_mapping_when_acl_manifest_exists(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a simple changeset whose ACL manifest is already available,
        // and a preexisting augmented root mapping representing a future
        // client-provided augmented manifest.
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a.txt", "initial")
            .commit()
            .await?;
        let manager = repo.repo_derived_data().manager();
        manager
            .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root], None)
            .await?;
        let provided_aug: RootHgAugmentedManifestId =
            BlobstoreBytes::from_bytes(Bytes::copy_from_slice(&[0x42; 20])).try_into()?;
        provided_aug
            .clone()
            .store_mapping(&ctx, &manager.derivation_context(None), root)
            .await?;

        // When: deriving through the manager.
        let derived = manager
            .derive::<RootHgAugmentedManifestId>(
                &ctx,
                root,
                None,
                derivation_queue_thrift::DerivationPriority::LOW,
            )
            .await?;

        // Then: the manager returns the preexisting augmented root mapping.
        assert_eq!(derived, provided_aug);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifest_derive_single_uses_mapped_hg_root(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a child changeset whose dependencies and parent augmented
        // manifest are already derived.
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a.txt", "initial")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("a.txt", "modified")
            .commit()
            .await?;
        let csids = vec![root, child];
        let manager = repo.repo_derived_data().manager();
        fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, csids.clone()).await?;
        manager
            .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
            .await?;
        manager
            .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![root], None)
            .await?;
        let parent_aug = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, root, None)
            .await?
            .context("Missing RootHgAugmentedManifestId for parent")?;
        let bonsai = Loadable::load(&child, &ctx, &repo.repo_blobstore).await?;
        let derivation_ctx = manager.derivation_context(None);

        // When: invoking the derive_single implementation.
        let derived = RootHgAugmentedManifestId::derive_single(
            &ctx,
            &derivation_ctx,
            bonsai,
            vec![parent_aug],
            None,
        )
        .await?;

        // Then: derive_single returns a loadable augmented manifest using the
        // canonical Hg root from the existing Bonsai-to-Hg mapping.
        let aug_envelope = Loadable::load(
            &derived.hg_augmented_manifest_id(),
            &ctx,
            &repo.repo_blobstore,
        )
        .await?;
        let mapped_child = manager
            .fetch_derived::<MappedHgChangesetId>(&ctx, child, None)
            .await?
            .context("Missing MappedHgChangesetId for child")?;
        let expected_root =
            Loadable::load(&mapped_child.hg_changeset_id(), &ctx, &repo.repo_blobstore)
                .await?
                .manifestid()
                .into_nodehash();
        assert_eq!(
            aug_envelope.augmented_manifest.hg_node_id, expected_root,
            "derive_single should use the mapped Hg root",
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifest_derive_batch_via_manager_persists_blobs(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a linear stack whose dependencies are derived, so the manager
        // can derive under its write-batched derivation context.
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a.txt", "initial")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("a.txt", "modified")
            .commit()
            .await?;
        let csids = vec![root, child];
        let manager = repo.repo_derived_data().manager();
        fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, csids.clone()).await?;
        manager
            .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
            .await?;

        // When: deriving augmented manifests through the manager.
        manager
            .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
            .await?;

        let derived = manager
            .fetch_derived_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
            .await?;
        let mapped = manager
            .fetch_derived_batch::<MappedHgChangesetId>(&ctx, csids.clone(), None)
            .await?;

        // Then: mappings point at augmented-manifest blobs that are loadable
        // from the repo blobstore after the manager-owned flush.
        for cs_id in &csids {
            let aug = derived
                .get(cs_id)
                .with_context(|| format!("Missing RootHgAugmentedManifestId for {cs_id}"))?;
            let aug_envelope =
                Loadable::load(&aug.hg_augmented_manifest_id(), &ctx, &repo.repo_blobstore).await?;
            let hg_cs_id = mapped
                .get(cs_id)
                .with_context(|| format!("Missing MappedHgChangesetId for {cs_id}"))?
                .hg_changeset_id();
            let expected_root = Loadable::load(&hg_cs_id, &ctx, &repo.repo_blobstore)
                .await?
                .manifestid()
                .into_nodehash();
            assert_eq!(
                aug_envelope.augmented_manifest.hg_node_id, expected_root,
                "derivation through the manager should persist the canonical root for {cs_id}",
            );
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifest_derive_batch_without_hgchangeset_mapping(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a linear stack with only the non-HgChangeset dependency prederived.
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a.txt", "initial")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("a.txt", "modified")
            .commit()
            .await?;
        let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
            .add_file("b.txt", "new file")
            .commit()
            .await?;
        let csids = vec![root, child, grandchild];
        let manager = repo.repo_derived_data().manager();
        manager
            .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
            .await?;
        for cs_id in &csids {
            assert!(
                manager
                    .fetch_derived::<MappedHgChangesetId>(&ctx, *cs_id, None)
                    .await?
                    .is_none(),
                "fixture should not prederive MappedHgChangesetId for {cs_id}",
            );
        }

        let mut bonsais = Vec::new();
        for cs_id in &csids {
            bonsais.push(Loadable::load(cs_id, &ctx, &repo.repo_blobstore).await?);
        }
        let derivation_ctx = manager.derivation_context(None);

        // When: invoking the derive_batch implementation directly.
        let derived =
            RootHgAugmentedManifestId::derive_batch(&ctx, &derivation_ctx, bonsais).await?;

        // Then: derivation produces content-derived roots and does not
        // create HgChangeset mappings as a side effect.
        for cs_id in &csids {
            let aug = derived
                .get(cs_id)
                .with_context(|| format!("Missing RootHgAugmentedManifestId for {cs_id}"))?;
            let aug_envelope =
                Loadable::load(&aug.hg_augmented_manifest_id(), &ctx, &repo.repo_blobstore).await?;
            assert_eq!(
                aug_envelope.augmented_manifest.hg_node_id,
                aug_envelope.augmented_manifest.computed_node_id,
                "no-Hg direct derivation should use content-derived root for {cs_id}",
            );
            assert_hgmanifest_blob_absent(
                &ctx,
                &repo,
                HgManifestId::new(aug_envelope.augmented_manifest.hg_node_id),
                *cs_id,
            )
            .await?;
            assert!(
                manager
                    .fetch_derived::<MappedHgChangesetId>(&ctx, *cs_id, None)
                    .await?
                    .is_none(),
                "derivation must not create MappedHgChangesetId for {cs_id}",
            );
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifest_derive_without_hgchangeset_mapping(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a linear stack with no derived data.
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a.txt", "initial")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file("dir/a.txt", "modified")
            .commit()
            .await?;
        let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
            .add_file("b.txt", "new file")
            .commit()
            .await?;
        let csids = vec![root, child, grandchild];
        let manager = repo.repo_derived_data().manager();

        // When: the manager derives augmented manifests for the head.
        manager
            .derive::<RootHgAugmentedManifestId>(
                &ctx,
                grandchild,
                None,
                derivation_queue_thrift::DerivationPriority::LOW,
            )
            .await?;

        // Then: no HgChangesets were derived, and each root is the Hg
        // manifest that later Hg derivation produces.
        for cs_id in &csids {
            assert!(
                manager
                    .fetch_derived::<MappedHgChangesetId>(&ctx, *cs_id, None)
                    .await?
                    .is_none(),
                "augmented derivation must not derive MappedHgChangesetId for {cs_id}",
            );
        }
        let derived = manager
            .fetch_derived_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
            .await?;
        for cs_id in &csids {
            let hg_cs_id = repo.derive_hg_changeset(&ctx, *cs_id).await?;
            let hg_manifest_id = Loadable::load(&hg_cs_id, &ctx, &repo.repo_blobstore)
                .await?
                .manifestid();
            assert_eq!(
                derived
                    .get(cs_id)
                    .map(|root| root.hg_augmented_manifest_id().into_nodehash()),
                Some(hg_manifest_id.into_nodehash()),
                "augmented root should match the Hg manifest for {cs_id}",
            );
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_hg_changeset_derive_derives_augmented_and_acl_manifests_first(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let control_repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a new two-commit stack with no derived data, and the same
        // stack in a second repo.
        let mut csids = Vec::new();
        for repo in [&repo, &control_repo] {
            let root = CreateCommitContext::new_root(&ctx, repo)
                .add_file("dir/a.txt", "initial")
                .commit()
                .await?;
            let child = CreateCommitContext::new(&ctx, repo, vec![root])
                .add_file("dir/a.txt", "modified")
                .add_file("b.txt", "new file")
                .commit()
                .await?;
            csids.push(vec![root, child]);
        }
        assert_eq!(csids[0], csids[1], "both repos have the same commits");
        let (root, child) = (csids[0][0], csids[0][1]);
        let manager = repo.repo_derived_data().manager();

        // When: the manager derives the Hg changeset of the head.
        let hg_cs_id = manager
            .derive::<MappedHgChangesetId>(
                &ctx,
                child,
                None,
                derivation_queue_thrift::DerivationPriority::LOW,
            )
            .await?
            .hg_changeset_id();

        // Then: the ACL and augmented manifests of both commits were derived.
        for cs_id in [root, child] {
            assert!(
                manager
                    .fetch_derived::<RootAclManifestId>(&ctx, cs_id, None)
                    .await?
                    .is_some(),
                "deriving the Hg changeset derives the ACL manifest of {cs_id}",
            );
            assert!(
                manager
                    .fetch_derived::<RootHgAugmentedManifestId>(&ctx, cs_id, None)
                    .await?
                    .is_some(),
                "deriving the Hg changeset derives the augmented manifest of {cs_id}",
            );
        }

        // And: the augmented manifest describes the Hg manifest that was
        // derived after it.
        let aug = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, child, None)
            .await?
            .context("Missing RootHgAugmentedManifestId for child")?;
        let aug_envelope =
            Loadable::load(&aug.hg_augmented_manifest_id(), &ctx, &repo.repo_blobstore).await?;
        let hg_manifest_id = Loadable::load(&hg_cs_id, &ctx, &repo.repo_blobstore)
            .await?
            .manifestid();
        assert_eq!(
            aug_envelope.augmented_manifest.hg_node_id,
            hg_manifest_id.into_nodehash(),
            "the augmented manifest has the Hg manifest's id",
        );

        // And: the Hg changeset is the one derived without the manager, which
        // knows nothing of dependencies.
        let control_ctx = control_repo
            .repo_derived_data()
            .manager()
            .derivation_context(None);
        let mut bonsais = Vec::new();
        for cs_id in [root, child] {
            bonsais.push(Loadable::load(&cs_id, &ctx, &control_repo.repo_blobstore).await?);
        }
        let control = MappedHgChangesetId::derive_batch(&ctx, &control_ctx, bonsais).await?;
        assert_eq!(
            control
                .get(&child)
                .map(MappedHgChangesetId::hg_changeset_id),
            Some(hg_cs_id),
            "the dependency does not change the Hg changeset",
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_stored_hg_changeset_is_returned_without_deriving_augmented_manifest(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a commit whose Hg changeset was stored outside derivation, as
        // when a client uploads it.
        let commit = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a.txt", "initial")
            .commit()
            .await?;
        fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![commit]).await?;
        let manager = repo.repo_derived_data().manager();
        let uploaded = manager
            .fetch_derived::<MappedHgChangesetId>(&ctx, commit, None)
            .await?
            .context("Missing uploaded MappedHgChangesetId")?;

        // When: the manager derives the Hg changeset.
        let derived = manager
            .derive::<MappedHgChangesetId>(
                &ctx,
                commit,
                None,
                derivation_queue_thrift::DerivationPriority::LOW,
            )
            .await?;

        // Then: it returns the stored Hg changeset and derives nothing else.
        assert_eq!(derived, uploaded);
        assert!(
            manager
                .fetch_derived::<RootHgAugmentedManifestId>(&ctx, commit, None)
                .await?
                .is_none(),
            "a stored Hg changeset does not derive the augmented manifest",
        );
        assert!(
            manager
                .fetch_derived::<RootAclManifestId>(&ctx, commit, None)
                .await?
                .is_none(),
            "a stored Hg changeset does not derive the ACL manifest",
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_batch_derive(fb: FacebookInit) -> Result<()> {
        verify_repo(fb, || Linear::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || BranchEven::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || BranchUneven::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || BranchWide::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || ManyDiamonds::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || ManyFilesDirs::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || MergeEven::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || MergeUneven::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || UnsharedMergeEven::get_repo::<TestRepo>(fb)).await?;
        verify_repo(fb, || UnsharedMergeUneven::get_repo::<TestRepo>(fb)).await?;
        // Create a repo with a few empty commits in a row
        verify_repo(fb, || async {
            let repo: TestRepo = test_repo_factory::build_empty(fb).await.unwrap();
            let ctx = CoreContext::test_mock(fb);
            let root_empty = CreateCommitContext::new_root(&ctx, &repo)
                .commit()
                .await
                .unwrap();
            let first_empty = CreateCommitContext::new(&ctx, &repo, vec![root_empty])
                .commit()
                .await
                .unwrap();
            let second_empty = CreateCommitContext::new(&ctx, &repo, vec![first_empty])
                .commit()
                .await
                .unwrap();
            let first_non_empty = CreateCommitContext::new(&ctx, &repo, vec![second_empty])
                .add_file("file", "a")
                .commit()
                .await
                .unwrap();
            let third_empty = CreateCommitContext::new(&ctx, &repo, vec![first_non_empty])
                .delete_file("file")
                .commit()
                .await
                .unwrap();
            let fourth_empty = CreateCommitContext::new(&ctx, &repo, vec![third_empty])
                .commit()
                .await
                .unwrap();
            let fifth_empty = CreateCommitContext::new(&ctx, &repo, vec![fourth_empty])
                .commit()
                .await
                .unwrap();

            tests_utils::bookmark(&ctx, &repo, "master")
                .set_to(fifth_empty)
                .await
                .unwrap();
            repo
        })
        .await?;

        verify_repo(fb, || async {
            let repo: TestRepo = test_repo_factory::build_empty(fb).await.unwrap();
            let ctx = CoreContext::test_mock(fb);
            let root = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("dir/subdir/to_replace", "one")
                .add_file("dir/subdir/file", "content")
                .add_file("somefile", "somecontent")
                .commit()
                .await
                .unwrap();
            let modify_unrelated = CreateCommitContext::new(&ctx, &repo, vec![root])
                .add_file("dir/subdir/file", "content2")
                .delete_file("somefile")
                .commit()
                .await
                .unwrap();
            let replace_file_with_dir =
                CreateCommitContext::new(&ctx, &repo, vec![modify_unrelated])
                    .delete_file("dir/subdir/to_replace")
                    .add_file("dir/subdir/to_replace/file", "newcontent")
                    .commit()
                    .await
                    .unwrap();

            tests_utils::bookmark(&ctx, &repo, "master")
                .set_to(replace_file_with_dir)
                .await
                .unwrap();
            repo
        })
        .await?;

        // Weird case - let's delete a file that was already replaced with a directory
        verify_repo(fb, || async {
            let repo: TestRepo = test_repo_factory::build_empty(fb).await.unwrap();
            let ctx = CoreContext::test_mock(fb);
            let root = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("dir/subdir/to_replace", "one")
                .commit()
                .await
                .unwrap();
            let replace_file_with_dir = CreateCommitContext::new(&ctx, &repo, vec![root])
                .delete_file("dir/subdir/to_replace")
                .add_file("dir/subdir/to_replace/file", "newcontent")
                .commit()
                .await
                .unwrap();
            let noop_delete = CreateCommitContext::new(&ctx, &repo, vec![replace_file_with_dir])
                .delete_file("dir/subdir/to_replace")
                .commit()
                .await
                .unwrap();
            let second_noop_delete = CreateCommitContext::new(&ctx, &repo, vec![noop_delete])
                .delete_file("dir/subdir/to_replace")
                .commit()
                .await
                .unwrap();

            tests_utils::bookmark(&ctx, &repo, "master")
                .set_to(second_noop_delete)
                .await
                .unwrap();
            repo
        })
        .await?;

        // Add renames
        verify_repo(fb, || async {
            let repo: TestRepo = test_repo_factory::build_empty(fb).await.unwrap();
            let ctx = CoreContext::test_mock(fb);
            let root = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("dir", "one")
                .commit()
                .await
                .unwrap();
            let renamed = CreateCommitContext::new(&ctx, &repo, vec![root])
                .add_file_with_copy_info("copied_dir", "one", (root, "dir"))
                .commit()
                .await
                .unwrap();
            let after_rename = CreateCommitContext::new(&ctx, &repo, vec![renamed])
                .add_file("new_file", "file")
                .commit()
                .await
                .unwrap();

            tests_utils::bookmark(&ctx, &repo, "master")
                .set_to(after_rename)
                .await
                .unwrap();
            repo
        })
        .await?;

        Ok(())
    }
}
