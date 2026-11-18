/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Compare the augmented manifests stored for uploaded commits with the ones
//! canonical derivation would build. Nothing is written, and the commits need
//! no augmented-manifest or ACL mapping, so this works on commits whose
//! envelopes were stored at tree upload and never derived.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::Loadable;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use cacheblob::MemWritesBlobstore;
use clap::Args;
use context::CoreContext;
use derived_data_manager::DerivedDataManager;
use futures::TryStreamExt;
use mercurial_derivation::derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents;
use mercurial_derivation::derive_hg_augmented_manifest::normalize_acl_root;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::sharded_augmented_manifest::HgAugmentedManifestEntry;
use mercurial_types::sharded_augmented_manifest::HgAugmentedManifestEnvelope;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use restricted_paths_common::NoopRestrictedPathsManifestIdStore;
use restricted_paths_common::RestrictedPathsConfigBased;

use super::Repo;
use super::verify_aug_direct::compare_envelopes;

#[derive(Args)]
pub(super) struct VerifyAugUploadArgs {
    /// Bonsai changesets to check. Each one's parents must have an hg
    /// changeset; list a stack parents first so their roots are reused.
    #[clap(long = "commit-id", short = 'i', required = true)]
    commit_ids: Vec<ChangesetId>,

    /// File whose lines start with hg tree ids, e.g. the trees a client
    /// uploaded. These are compared even when their parent's entry for them
    /// already matches, which checks their envelope bytes, not just their
    /// digest.
    #[clap(long)]
    trees_file: Option<String>,

    /// Build the canonical side with no augmented parents, so every directory
    /// is rebuilt instead of reused, and compare every directory in the tree.
    #[clap(long)]
    from_scratch: bool,
}

/// Envelopes are keyed by hg node id, so a recomputed envelope lands on the
/// key the stored one already occupies. `MemWritesBlobstore` writes reached
/// through `RepoBlobstore` are if-absent against the real blobstore, which
/// would drop the recomputed envelope and read the stored one back. Every
/// write here goes to the in-memory overlay instead.
#[derive(Clone, Debug)]
pub(super) struct OverwritingOverlay(pub(super) Arc<MemWritesBlobstore<Arc<dyn Blobstore>>>);

impl std::fmt::Display for OverwritingOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "OverwritingOverlay<{}>", self.0)
    }
}

#[async_trait]
impl Blobstore for OverwritingOverlay {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.0.get(ctx, key).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.0.is_present(ctx, key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.0.unlink(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        _put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.0
            .put_explicit(ctx, key, value, PutBehaviour::Overwrite)
            .await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.0
            .put_explicit(ctx, key, value, PutBehaviour::Overwrite)
            .await
    }
}

async fn subentries(
    ctx: &CoreContext,
    blobstore: &RepoBlobstore,
    envelope: &HgAugmentedManifestEnvelope,
) -> Result<BTreeMap<MPathElement, HgAugmentedManifestEntry>> {
    envelope
        .augmented_manifest
        .subentries
        .clone()
        .into_entries(ctx, blobstore)
        .and_then(|(name, entry)| async move { Ok((MPathElement::from_smallvec(name)?, entry)) })
        .try_collect()
        .await
}

struct Checker<'a> {
    ctx: &'a CoreContext,
    stored: RepoBlobstore,
    computed: RepoBlobstore,
    trees_to_compare: HashSet<HgNodeHash>,
    compare_all: bool,
    mismatches: Vec<String>,
    dirs_compared: usize,
}

impl Checker<'_> {
    async fn compare_dir(&mut self, cs_id: ChangesetId, path: MPath, hg: HgNodeHash) -> Result<()> {
        let id = HgAugmentedManifestId::new(hg);
        let stored = id
            .load(self.ctx, &self.stored)
            .await
            .with_context(|| format!("loading stored envelope {hg} at {path}"))?;
        let computed = id
            .load(self.ctx, &self.computed)
            .await
            .with_context(|| format!("loading computed envelope {hg} at {path}"))?;
        self.dirs_compared += 1;

        if let Some(m) = compare_envelopes(cs_id, &computed, &stored) {
            self.mismatches.push(format!(
                "{cs_id} {path} ({hg}): {} computed={} stored={}",
                m.field, m.computed, m.expected
            ));
        } else if computed.clone().into_blob() != stored.clone().into_blob() {
            self.mismatches.push(format!(
                "{cs_id} {path} ({hg}): same header and digest, different envelope bytes"
            ));
        }

        let stored_entries = subentries(self.ctx, &self.stored, &stored).await?;
        let computed_entries = subentries(self.ctx, &self.computed, &computed).await?;
        let names: BTreeSet<MPathElement> = stored_entries
            .keys()
            .chain(computed_entries.keys())
            .cloned()
            .collect();

        for name in names {
            let child_path = path.join(&name);
            match (stored_entries.get(&name), computed_entries.get(&name)) {
                (
                    Some(HgAugmentedManifestEntry::DirectoryNode(s)),
                    Some(HgAugmentedManifestEntry::DirectoryNode(c)),
                ) => {
                    if s != c {
                        self.mismatches.push(format!(
                            "{cs_id} {child_path}: directory entry differs computed={c:?} stored={s:?}"
                        ));
                    }
                    // A different tree id is a different hg tree, which the
                    // entry mismatch above already reports.
                    if s.treenode == c.treenode
                        && (s != c
                            || self.compare_all
                            || self.trees_to_compare.contains(&s.treenode))
                    {
                        Box::pin(self.compare_dir(cs_id, child_path, s.treenode)).await?;
                    }
                }
                (s, c) if s != c => {
                    self.mismatches.push(format!(
                        "{cs_id} {child_path}: entry differs computed={c:?} stored={s:?}"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

async fn root_hg_manifest(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<HgNodeHash> {
    let hg_cs_id = repo
        .bonsai_hg_mapping()
        .get_hg_from_bonsai(ctx, cs_id)
        .await?
        .ok_or_else(|| anyhow!("no hg changeset for {cs_id}"))?;
    let hg_cs = hg_cs_id
        .load(ctx, repo.repo_blobstore())
        .await
        .with_context(|| format!("loading hg changeset {hg_cs_id} for {cs_id}"))?;
    Ok(hg_cs.manifestid().into_nodehash())
}

pub(super) async fn verify_aug_upload(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    args: VerifyAugUploadArgs,
) -> Result<()> {
    let trees_to_compare = match &args.trees_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading {path}"))?
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(|hex| hex.parse::<HgNodeHash>())
            .collect::<Result<HashSet<_>>>()?,
        None => HashSet::new(),
    };

    let mut overlay = None;
    let computed =
        RepoBlobstore::new_with_wrapped_inner_blobstore(repo.repo_blobstore().clone(), |inner| {
            let mem = Arc::new(MemWritesBlobstore::new(inner));
            overlay = Some(mem.clone());
            Arc::new(OverwritingOverlay(mem))
        });
    let overlay = overlay.context("repo blobstore wrapper was not called")?;

    // Recording restricted-path manifest ids is a write, so it goes nowhere.
    let derivation_ctx = manager.derivation_context(None);
    let restricted_paths = Arc::new(RestrictedPathsConfigBased::new(
        derivation_ctx.restricted_paths().config().clone(),
        Arc::new(NoopRestrictedPathsManifestIdStore::new(
            derivation_ctx.repo_id(),
        )),
        None,
    ));
    let no_store_manager = manager
        .with_replaced_blobstore(computed.clone())
        .with_replaced_restricted_paths(restricted_paths.clone());

    let mut checker = Checker {
        ctx,
        stored: repo.repo_blobstore().clone(),
        computed: computed.clone(),
        trees_to_compare,
        compare_all: args.from_scratch,
        mismatches: Vec::new(),
        dirs_compared: 0,
    };

    let mut roots: HashMap<ChangesetId, HgNodeHash> = HashMap::new();
    for cs_id in args.commit_ids {
        let bonsai = cs_id.load(ctx, repo.repo_blobstore()).await?;
        for wanted in bonsai.parents().chain(std::iter::once(cs_id)) {
            if let Entry::Vacant(e) = roots.entry(wanted) {
                e.insert(root_hg_manifest(ctx, repo, wanted).await?);
            }
        }
        let root = roots[&cs_id];
        let aug_parents = if args.from_scratch {
            Vec::new()
        } else {
            bonsai
                .parents()
                .map(|parent| HgAugmentedManifestId::new(roots[&parent]))
                .collect()
        };

        let acl_root = no_store_manager
            .derive_locally::<RootAclManifestId>(ctx, cs_id, None)
            .await
            .with_context(|| format!("deriving the ACL manifest in memory for {cs_id}"))?;
        let acl_overlay = normalize_acl_root(&acl_root)?;

        // The path canonical derivation takes for a commit with an hg mapping,
        // which every uploaded commit has.
        let computed_root = derive_from_hg_manifest_and_parents(
            ctx,
            &computed,
            HgManifestId::new(root),
            aug_parents,
            &HashMap::new(),
            &restricted_paths,
            acl_overlay,
        )
        .await
        .with_context(|| format!("deriving the augmented manifest in memory for {cs_id}"))?;
        if computed_root.into_nodehash() != root {
            checker.mismatches.push(format!(
                "{cs_id}: computed root {} is not the uploaded root {root}",
                computed_root.into_nodehash()
            ));
        }

        let (dirs_before, mismatches_before) = (checker.dirs_compared, checker.mismatches.len());
        checker.compare_dir(cs_id, MPath::ROOT, root).await?;
        // A count that stays at zero would mean the computed side never
        // reached the overlay and was read back from storage.
        let overlay_blobs = overlay
            .get_cache()
            .lock()
            .expect("overlay cache lock should not be poisoned")
            .len();
        println!(
            "{cs_id} root={root} acl_overlay={acl_overlay:?} overlay_blobs={overlay_blobs} dirs_compared={} mismatches={}",
            checker.dirs_compared - dirs_before,
            checker.mismatches.len() - mismatches_before,
        );
    }

    for mismatch in &checker.mismatches {
        println!("MISMATCH {mismatch}");
    }
    println!(
        "done: dirs_compared={} mismatches={}",
        checker.dirs_compared,
        checker.mismatches.len()
    );
    Ok(())
}
