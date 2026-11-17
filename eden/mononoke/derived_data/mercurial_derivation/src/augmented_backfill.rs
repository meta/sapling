/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Backfill derivation of augmented manifests from a changeset's existing Hg
//! manifest. It needs no parent augmented manifests, so backfills use it only
//! in repos that already have Hg changesets.

use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::Storable;
use blobstore::StoreLoadable;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivableUntopologically;
use derived_data_manager::DerivationContext;
use derived_data_manager::dependencies;
use filestore::FetchKey;
use futures::future;
use manifest::Entry;
use manifest::derive_manifest_from_predecessor;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestEnvelope;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mercurial_types::ShardedHgAugmentedManifest;
use mercurial_types::sharded_augmented_manifest::HgAugmentedDirectoryNode;
use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
use mononoke_types::BonsaiChangeset;
use mononoke_types::DerivableUntopologicallyVariant;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types::typed_hash::AclManifestId;

use crate::access_labels;
use crate::acl_overlay_manifest::AclOverlayHgManifestId;
use crate::derive_hg_augmented_manifest::assert_root_acl_pointer_invariant;
use crate::mapping::MappedHgChangesetId;
use crate::mapping::RootHgAugmentedManifestId;

#[async_trait]
impl DerivableUntopologically for RootHgAugmentedManifestId {
    const DERIVABLE_UNTOPOLOGICALLY_VARIANT: DerivableUntopologicallyVariant =
        DerivableUntopologicallyVariant::HgAugmentedManifests;
    type PredecessorDependencies = dependencies![MappedHgChangesetId, RootAclManifestId];

    async fn unsafe_derive_untopologically(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsai: BonsaiChangeset,
    ) -> Result<Self> {
        let ctx = &ctx.with_access_label(access_labels::AUGMENTED_BACKFILL_FROM_HG);
        let csid = bonsai.get_changeset_id();
        let (hg_changeset_id, acl_root) = future::try_join(
            async {
                Ok(derivation_ctx
                    .fetch_dependency::<MappedHgChangesetId>(ctx, csid)
                    .await?
                    .hg_changeset_id())
            },
            derivation_ctx.fetch_dependency::<RootAclManifestId>(ctx, csid),
        )
        .await?;
        let acl_root_overlay = crate::derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;
        let hg_manifest_id = hg_changeset_id
            .load(ctx, derivation_ctx.blobstore())
            .await?
            .manifestid();
        let root = derive_from_full_hg_manifest(
            ctx.clone(),
            Arc::clone(derivation_ctx.blobstore()),
            hg_manifest_id,
            acl_root_overlay,
        )
        .await?;

        // Track restricted paths for the derived HgAugmentedManifest
        crate::derive_hg_augmented_manifest::track_all_restricted_paths(
            ctx,
            derivation_ctx.restricted_paths(),
            hg_changeset_id,
            root,
            Arc::clone(derivation_ctx.blobstore()),
        )
        .await?;

        Ok(Self::new(root))
    }
}

pub async fn derive_from_full_hg_manifest(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    hg_manifest_id: HgManifestId,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<HgAugmentedManifestId> {
    let predecessor = AclOverlayHgManifestId {
        hg_manifest_id,
        acl_manifest_id: acl_root_overlay,
    };
    let root_entry = derive_manifest_from_predecessor(
        ctx.clone(),
        blobstore.clone(),
        predecessor,
        {
            cloned!(ctx, blobstore);
            move |tree_info| {
                cloned!(ctx, blobstore);
                async move {
                    let acl_manifest_directory_id = tree_info.predecessor.acl_manifest_id;
                    let hg_manifest = tree_info
                        .predecessor
                        .hg_manifest_id
                        .load(&ctx, &blobstore)
                        .await?;
                    let entries = tree_info.subentries.into_iter().map(|(elem, ((), entry))| {
                        let entry = match entry {
                            Entry::Tree(tree) => HgAugmentedManifestEntry::DirectoryNode(tree),
                            Entry::Leaf(leaf) => HgAugmentedManifestEntry::FileNode(leaf),
                        };
                        (elem, entry)
                    });
                    let subentries =
                        ShardedMapV2Node::from_entries(&ctx, &blobstore, entries).await?;
                    let augmented_manifest = ShardedHgAugmentedManifest {
                        hg_node_id: tree_info.predecessor.hg_manifest_id.into_nodehash(),
                        p1: hg_manifest.p1(),
                        p2: hg_manifest.p2(),
                        computed_node_id: hg_manifest.computed_node_id(),
                        subentries,
                        acl_manifest_directory_id,
                    };

                    assert_root_acl_pointer_invariant(
                        &augmented_manifest.acl_manifest_directory_id,
                    )?;

                    let (augmented_manifest_id, augmented_manifest_size) = augmented_manifest
                        .clone()
                        .compute_content_addressed_digest(&ctx, &blobstore)
                        .await?;
                    let envelope = HgAugmentedManifestEnvelope {
                        augmented_manifest_id,
                        augmented_manifest_size,
                        augmented_manifest,
                    };
                    envelope.store(&ctx, &blobstore).await?;
                    let entry = HgAugmentedDirectoryNode {
                        treenode: tree_info.predecessor.hg_manifest_id.into_nodehash(),
                        augmented_manifest_id,
                        augmented_manifest_size,
                        acl_manifest_directory_id,
                    };
                    Ok(((), entry))
                }
            }
        },
        {
            cloned!(ctx, blobstore);
            move |leaf_info| {
                cloned!(ctx, blobstore);
                async move {
                    let (file_type, filenode_id) = leaf_info.predecessor;
                    let filenode = filenode_id.load(&ctx, &blobstore).await?;
                    let metadata = filestore::get_metadata(
                        &blobstore,
                        &ctx,
                        &FetchKey::Canonical(filenode.content_id()),
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Missing metadata for {filenode_id}"))?;
                    let hg_augmented_file_leaf_node = HgAugmentedFileLeafNode {
                        file_type,
                        filenode: filenode_id.into_nodehash(),
                        total_size: metadata.total_size,
                        content_blake3: metadata.seeded_blake3,
                        content_sha1: metadata.sha1,
                        file_header_metadata: if filenode.metadata().is_empty() {
                            None
                        } else {
                            Some(filenode.metadata().clone())
                        },
                    };
                    Ok(((), hg_augmented_file_leaf_node))
                }
            }
        },
    )
    .await?;
    Ok(HgAugmentedManifestId::new(root_entry.treenode))
}
