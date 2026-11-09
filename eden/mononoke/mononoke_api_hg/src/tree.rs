/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgBlobEnvelope;
use mercurial_types::HgManifestEnvelope;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::HgParents;
use mercurial_types::HgPreloadedAugmentedManifest;
use mercurial_types::fetch_augmented_manifest_envelope_opt;
use mercurial_types::fetch_manifest_envelope;
use mercurial_types::fetch_manifest_envelope_opt;
use mononoke_api::MononokeRepo;
use mononoke_api::errors::MononokeError;
use mononoke_types::MPathElement;
use mononoke_types::hash::Blake3;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use restricted_paths::ManifestRestrictionCheckResult;
use restricted_paths::ManifestType;
use restricted_paths::RestrictedManifestId;
use restricted_paths::RestrictedPathsArc;
use restricted_paths::RestrictedPathsAuthorizationError;
use restricted_paths::RestrictedPathsError;
use revisionstore_types::Metadata;

use super::HgDataContext;
use super::HgDataId;
use super::HgRepoContext;

const USE_RESTRICTED_PATHS_FOR_AUGMENTED_TREE_ACL_METADATA: &str =
    "scm/mononoke:use_restricted_paths_for_augmented_tree_acl_metadata";

const DENY_ORIGINAL_HG_MANIFEST: &str = "scm/mononoke:deny_original_hg_manifest";

#[derive(Clone)]
pub struct HgTreeContext<R> {
    #[allow(dead_code)]
    repo_ctx: HgRepoContext<R>,
    envelope: HgManifestEnvelope,
}

impl<R: MononokeRepo> HgTreeContext<R> {
    /// Create a new `HgTreeContext`, representing a single tree manifest node.
    ///
    /// The tree node must exist in the repository. To construct an `HgTreeContext`
    /// for a tree node that may not exist, use `new_check_exists`.
    pub async fn new(
        repo_ctx: HgRepoContext<R>,
        manifest_id: HgManifestId,
    ) -> Result<Self, MononokeError> {
        let ctx = repo_ctx.ctx();
        let blobstore = repo_ctx.repo().repo_blobstore();
        let envelope = fetch_manifest_envelope(ctx, blobstore, manifest_id).await?;
        Ok(Self { repo_ctx, envelope })
    }

    pub async fn new_check_exists(
        repo_ctx: HgRepoContext<R>,
        manifest_id: HgManifestId,
    ) -> Result<Option<Self>, MononokeError> {
        let ctx = repo_ctx.ctx();

        // Killswitch (T271591213): tented repos must never fall back to the
        // original (non-augmented) Hg manifest, which lacks Path ACL
        // restriction metadata. When the JK is enabled for the repo, refuse
        // the request instead of serving the unprotected manifest.
        let repo_name = repo_ctx.repo().repo_identity().name();
        if justknobs::eval(DENY_ORIGINAL_HG_MANIFEST, None, Some(repo_name)) {
            return Err(MononokeError::InvalidRequest(format!(
                "original (non-augmented) Hg manifest is disabled for repo {repo_name}; \
                 request augmented manifests (aux data) instead"
            )));
        }

        let blobstore = repo_ctx.repo().repo_blobstore();
        let envelope = fetch_manifest_envelope_opt(ctx, blobstore, manifest_id).await?;

        let manifest_id = RestrictedManifestId::new(manifest_id.as_bytes().into());
        restricted_paths::spawn_enforce_restricted_manifest_access(
            ctx,
            repo_ctx.repo_ctx().repo().restricted_paths_arc().clone(),
            manifest_id,
            ManifestType::Hg,
            "hg_tree_context_new_check_exists",
            None,
        )
        .await?;

        Ok(envelope.map(move |envelope| Self { repo_ctx, envelope }))
    }

    /// Get the content for this tree manifest node in the format expected
    /// by Mercurial's data storage layer.
    pub fn content_bytes(&self) -> Bytes {
        self.envelope.contents().clone()
    }

    pub fn into_blob_manifest(self) -> anyhow::Result<mercurial_types::blobs::HgBlobManifest> {
        mercurial_types::blobs::HgBlobManifest::parse(self.envelope)
    }
}

#[derive(Clone)]
pub struct HgAugmentedTreeContext<R> {
    #[allow(dead_code)]
    repo_ctx: HgRepoContext<R>,
    preloaded_manifest: HgPreloadedAugmentedManifest,
    restriction_ctx: HgAugmentedTreeRestrictionContext<R>,
}

impl<R: MononokeRepo> HgAugmentedTreeContext<R> {
    /// Create a new `HgAugmentedTreeContext`, representing a single augmented tree manifest node.
    pub async fn new_check_exists(
        repo_ctx: HgRepoContext<R>,
        augmented_manifest_id: HgAugmentedManifestId,
    ) -> Result<Option<Self>, MononokeError> {
        let ctx = repo_ctx.ctx();
        let blobstore = repo_ctx.repo().repo_blobstore();
        let envelope =
            fetch_augmented_manifest_envelope_opt(ctx, blobstore, augmented_manifest_id).await?;

        let manifest_id = RestrictedManifestId::new(augmented_manifest_id.as_bytes().into());
        restricted_paths::spawn_enforce_restricted_manifest_access(
            ctx,
            repo_ctx.repo_ctx().repo().restricted_paths_arc().clone(),
            manifest_id,
            ManifestType::HgAugmented,
            "hg_augmented_tree_context_new_check_exists",
            None,
        )
        .await?;

        if let Some(envelope) = envelope {
            let preloaded_manifest =
                HgPreloadedAugmentedManifest::load_from_sharded(envelope, ctx, blobstore).await?;
            Ok(Some(Self {
                restriction_ctx: HgAugmentedTreeRestrictionContext::new(
                    repo_ctx.clone(),
                    augmented_manifest_id,
                )?,
                repo_ctx,
                preloaded_manifest,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn augmented_manifest_id(&self) -> Blake3 {
        self.preloaded_manifest.augmented_manifest_id
    }

    pub fn augmented_manifest_size(&self) -> u64 {
        self.preloaded_manifest.augmented_manifest_size
    }

    /// Whether this directory itself is a restriction root (has a `.slacl` file).
    /// Waypoints (ancestors of restriction roots) return `false`.
    pub async fn is_restricted(&self) -> Result<bool, MononokeError> {
        self.restriction_ctx
            .is_restricted(&self.preloaded_manifest)
            .await
    }

    pub async fn children_restrictions(
        &self,
        max_concurrent_fetches: usize,
    ) -> Result<HashMap<MPathElement, bool>, MononokeError> {
        if !self.restriction_ctx.use_restricted_paths_for_acl_metadata {
            return Ok(self.preloaded_manifest.child_restriction_map.clone());
        }

        if !self
            .repo_ctx
            .repo()
            .restricted_paths_arc()
            .may_have_restricted_paths()
        {
            return Ok(HashMap::new());
        }

        let child_restriction_data = self
            .augmented_children_entries()
            .filter_map(|(name, child_entry)| match child_entry {
                HgAugmentedManifestEntry::DirectoryNode(tree) => Some((
                    name.clone(),
                    self.preloaded_manifest
                        .child_restriction_map
                        .get(name)
                        .copied(),
                    HgAugmentedManifestId::new(tree.treenode),
                )),
                HgAugmentedManifestEntry::FileNode(_) => None,
            })
            .collect::<Vec<_>>();

        let child_restrictions = stream::iter(child_restriction_data)
            .map(|(name, preloaded_child_is_restricted, child_mfid)| {
                let restriction_ctx = self.restriction_ctx.clone();
                async move {
                    restriction_ctx
                        .child_is_restricted(name, preloaded_child_is_restricted, child_mfid)
                        .await
                }
            })
            .buffer_unordered(max_concurrent_fetches)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(child_restrictions.into_iter().collect())
    }

    pub fn augmented_children_entries(
        &self,
    ) -> impl Iterator<Item = &(MPathElement, HgAugmentedManifestEntry)> {
        self.preloaded_manifest.children_augmented_metadata.iter()
    }

    /// Get the content for this tree manifest node in the format expected
    /// by Mercurial's data storage layer.
    pub fn content_bytes(&self) -> Bytes {
        self.preloaded_manifest.contents.clone()
    }
}

#[async_trait]
impl<R: MononokeRepo> HgDataContext<R> for HgTreeContext<R> {
    type NodeId = HgManifestId;

    /// Get the manifest node hash (HgManifestId) for this tree.
    ///
    /// This should be same as the HgManifestId specified when this context was created,
    /// but the value returned here comes from the data loaded from Mononoke.
    fn node_id(&self) -> HgManifestId {
        HgManifestId::new(self.envelope.node_id())
    }

    /// Get the parents of this tree node in a strongly-typed manner.
    ///
    /// Useful for implementing anything that needs to traverse the history
    /// of tree nodes, or otherwise needs to use make further queries using
    /// the returned `HgManifestId`s.
    fn parents(&self) -> (Option<HgManifestId>, Option<HgManifestId>) {
        let (p1, p2) = self.envelope.parents();
        (p1.map(HgManifestId::new), p2.map(HgManifestId::new))
    }

    /// Get the parents of this tree node in a format that can be easily
    /// sent to the Mercurial client as part of a serialized response.
    fn hg_parents(&self) -> HgParents {
        self.envelope.get_parents()
    }

    /// The manifest envelope actually contains the underlying tree bytes
    /// inline, so they can be accessed synchronously and infallibly with the
    /// `content_bytes` method. This method just wraps the bytes in a TryFuture
    /// that immediately succeeds. Note that tree blobs don't have associated
    /// metadata so we just return the default value here.
    async fn content(&self) -> Result<(Bytes, Metadata), MononokeError> {
        Ok((self.content_bytes(), Metadata::default()))
    }
}

#[async_trait]
impl<R: MononokeRepo> HgDataContext<R> for HgAugmentedTreeContext<R> {
    type NodeId = HgManifestId;

    /// Get the manifest node hash (HgAugmentedManifestId) for this tree.
    ///
    /// This should be same as the HgAugmentedManifestId specified when this context was created,
    /// but the value returned here comes from the data loaded from Mononoke.
    fn node_id(&self) -> HgManifestId {
        HgManifestId::new(self.preloaded_manifest.hg_node_id)
    }

    /// Get the parents of this tree node in a strongly-typed manner.
    ///
    /// Useful for implementing anything that needs to traverse the history
    /// of tree nodes, or otherwise needs to use make further queries using
    /// the returned `HgManifestId`s.
    fn parents(&self) -> (Option<HgManifestId>, Option<HgManifestId>) {
        (
            self.preloaded_manifest.p1.map(HgManifestId::new),
            self.preloaded_manifest.p2.map(HgManifestId::new),
        )
    }

    /// Get the parents of this tree node in a format that can be easily
    /// sent to the Mercurial client as part of a serialized response.
    fn hg_parents(&self) -> HgParents {
        HgParents::new(self.preloaded_manifest.p1, self.preloaded_manifest.p2)
    }

    async fn content(&self) -> Result<(Bytes, Metadata), MononokeError> {
        Ok((self.content_bytes(), Metadata::default()))
    }
}

#[async_trait]
impl<R: MononokeRepo> HgDataId<R> for HgManifestId {
    type Context = HgTreeContext<R>;

    fn from_node_hash(hash: HgNodeHash) -> Self {
        HgManifestId::new(hash)
    }

    async fn context(
        self,
        repo: HgRepoContext<R>,
    ) -> Result<Option<HgTreeContext<R>>, MononokeError> {
        HgTreeContext::new_check_exists(repo, self).await
    }
}

#[async_trait]
impl<R: MononokeRepo> HgDataId<R> for HgAugmentedManifestId {
    type Context = HgAugmentedTreeContext<R>;

    fn from_node_hash(hash: HgNodeHash) -> Self {
        HgAugmentedManifestId::new(hash)
    }

    async fn context(
        self,
        repo: HgRepoContext<R>,
    ) -> Result<Option<HgAugmentedTreeContext<R>>, MononokeError> {
        HgAugmentedTreeContext::new_check_exists(repo, self).await
    }
}

/// Context for querying restriction metadata about an Hg manifest node.
///
/// Like `ChangesetPathRestrictionContext` in mononoke_api, this type does NOT
/// enforce Path ACL access checks in its constructor. This avoids the circular
/// dependency where callers would need permission to instantiate the context
/// that checks permissions.
///
/// This type never returns manifest content — only restriction metadata
/// (ACLs, access checks).
#[derive(Clone)]
pub struct HgAugmentedTreeRestrictionContext<R> {
    repo_ctx: HgRepoContext<R>,
    manifest_id: HgAugmentedManifestId,
    // Whether the restricted_paths facet primitives should be used to get
    // restriction information or it should just be fetched directly from the
    // HgAugmentedManifest information (populated from the AclManifest data).
    // This is set by a JK to ensure that we can roll-back in case there are any
    // performance regressions.
    use_restricted_paths_for_acl_metadata: bool,
}

impl<R: MononokeRepo> HgAugmentedTreeRestrictionContext<R> {
    /// Create a new restriction context for the given manifest ID.
    ///
    /// Checks repo read access but does NOT enforce Path ACL checks.
    pub async fn new_check_exists(
        repo_ctx: HgRepoContext<R>,
        manifest_id: HgAugmentedManifestId,
    ) -> Result<Self, MononokeError> {
        repo_ctx
            .repo_ctx()
            .authorization_context()
            .require_full_repo_read(repo_ctx.ctx(), repo_ctx.repo_ctx().repo())
            .await?;

        Ok(Self::new(repo_ctx, manifest_id)?)
    }

    fn new(repo_ctx: HgRepoContext<R>, manifest_id: HgAugmentedManifestId) -> Result<Self> {
        let repo_name = repo_ctx.repo().repo_identity().name();
        let use_restricted_paths_for_acl_metadata = justknobs::eval(
            USE_RESTRICTED_PATHS_FOR_AUGMENTED_TREE_ACL_METADATA,
            None,
            Some(repo_name),
        );

        Ok(Self {
            repo_ctx,
            manifest_id,
            use_restricted_paths_for_acl_metadata,
        })
    }

    async fn is_restricted(
        &self,
        preloaded_manifest: &HgPreloadedAugmentedManifest,
    ) -> Result<bool, MononokeError> {
        if !self.use_restricted_paths_for_acl_metadata {
            return Ok(preloaded_manifest.is_restricted);
        }

        self.is_manifest_restricted(preloaded_manifest.is_restricted)
            .await
    }

    async fn child_is_restricted(
        &self,
        name: MPathElement,
        preloaded_child_is_restricted: Option<bool>,
        child_mfid: HgAugmentedManifestId,
    ) -> Result<(MPathElement, bool), MononokeError> {
        let child_ctx = self.child_ctx(child_mfid);
        let is_restricted = child_ctx
            .is_manifest_restricted(preloaded_child_is_restricted.unwrap_or(false))
            .await?;
        Ok((name, is_restricted))
    }

    /// Whether the caller may fetch this manifest: the enforcement decision a
    /// tree fetch makes, made without fetching the manifest. `Ok(Err(_))` is
    /// the denial the fetch would return.
    pub async fn check_fetch_access(
        &self,
    ) -> Result<Result<(), RestrictedPathsAuthorizationError>, MononokeError> {
        let manifest_id = RestrictedManifestId::new(self.manifest_id.as_bytes().into());
        match restricted_paths::spawn_enforce_restricted_manifest_access(
            self.repo_ctx.ctx(),
            self.repo_ctx.repo().restricted_paths_arc().clone(),
            manifest_id,
            ManifestType::HgAugmented,
            "check_manifest_permission",
            None,
        )
        .await
        {
            Ok(()) => Ok(Ok(())),
            Err(RestrictedPathsError::AuthorizationError(denial)) => Ok(Err(denial)),
            Err(err) => Err(err.into()),
        }
    }

    /// Query restriction and authorization check results for this manifest node.
    ///
    /// Returns the restriction info for this specific manifest, or an empty
    /// vector if the manifest is not at a restricted path.
    ///
    /// Unlike `ChangesetPathRestrictionContext::restriction_info`,
    /// which can traverse the AclManifest from the root path and aggregate all
    /// path restrictions, this primitive can only access the AclManifest from
    /// the given manifest ids. It doesn't have visibility into its parents,
    /// so it will only return `ManifestRestrictionInfo` if the manifest
    /// belongs to a restriction root.
    ///
    /// This is acceptable **under an important assumption**: in order to fetch
    /// any child manifest, the client must already have access to the parent
    /// manifest, which means they have permission to access the directory.
    pub async fn restriction_check(
        &self,
    ) -> Result<Vec<ManifestRestrictionCheckResult>, MononokeError> {
        let restricted_paths = self.repo_ctx.repo().restricted_paths_arc();

        if !restricted_paths.may_have_restricted_paths() {
            return Ok(vec![]);
        }

        let manifest_id_bytes = RestrictedManifestId::new(self.manifest_id.as_bytes().into());
        restricted_paths
            .get_manifest_restriction_check(
                self.repo_ctx.ctx(),
                &manifest_id_bytes,
                &ManifestType::HgAugmented,
            )
            .await
            .map_err(MononokeError::from)
    }

    /// Query restriction metadata for this manifest node without checking authorization.
    async fn is_manifest_restricted(
        &self,
        preloaded_is_restricted: bool,
    ) -> Result<bool, MononokeError> {
        let restricted_paths = self.repo_ctx.repo().restricted_paths_arc();

        let manifest_id_bytes = RestrictedManifestId::new(self.manifest_id.as_bytes().into());
        restricted_paths
            .is_restricted_manifest(
                self.repo_ctx.ctx(),
                &manifest_id_bytes,
                &ManifestType::HgAugmented,
                preloaded_is_restricted,
            )
            .await
            .map_err(MononokeError::from)
    }

    /// Create a restriction context for a child directory after repo read access was checked.
    fn child_ctx(&self, child_mfid: HgAugmentedManifestId) -> Self {
        Self {
            repo_ctx: self.repo_ctx.clone(),
            manifest_id: child_mfid,
            use_restricted_paths_for_acl_metadata: self.use_restricted_paths_for_acl_metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Arc;

    use acl_manifest::RootAclManifestId;
    use anyhow::Context;
    use blobstore::Loadable;
    use clientinfo::ClientEntryPoint;
    use clientinfo::ClientInfo;
    use clientinfo::ClientRequestInfo;
    use context::CoreContext;
    use context::SessionContainer;
    use fbinit::FacebookInit;
    use fixtures::Linear;
    use fixtures::TestRepoFixture;
    use maplit::hashmap;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_derivation::MappedHgChangesetId;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mercurial_types::HgAugmentedManifestEnvelope;
    use mercurial_types::NULL_HASH;
    use metaconfig_types::AclManifestMode;
    use metaconfig_types::EnforcementConditionSet;
    use metaconfig_types::PathRestrictionMetadata;
    use metaconfig_types::RequestMatchers;
    use metaconfig_types::RestrictedPathsConfig;
    use metaconfig_types::RestrictedPathsManifestIdStoreConfig;
    use metadata::Metadata;
    use mononoke_api::repo::Repo;
    use mononoke_api::repo::RepoContext;
    use mononoke_api::specifiers::HgChangesetId;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::NonRootMPath;
    use mononoke_types::RepositoryId;
    use mononoke_types::path::MPath;
    use permission_checker::Acl;
    use permission_checker::Acls;
    use permission_checker::InternalAclProvider;
    use permission_checker::MononokeIdentity;
    use permission_checker::MononokeIdentitySet;
    use pretty_assertions::assert_eq;
    use repo_derived_data::RepoDerivedDataArc;
    use repo_derived_data::RepoDerivedDataRef;
    use restricted_paths::RestrictedPaths;
    use restricted_paths::RestrictedPathsConfigBased;
    use restricted_paths::SqlRestrictedPathsManifestIdStoreBuilder;
    use scuba_ext::MononokeScubaSampleBuilder;
    use sql_construct::SqlConstruct;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::RepoContextHgExt;

    #[mononoke::fbinit_test]
    async fn test_hg_tree_context(fb: FacebookInit) -> Result<(), MononokeError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = Arc::new(Linear::get_repo::<Repo>(fb).await);
        let rctx = RepoContext::new_test(ctx.clone(), repo.clone()).await?;

        // Get the HgManifestId of the root tree manifest for a commit in this repo.
        // (Commit hash was found by inspecting the source of the `fixtures` crate.)
        let hg_cs_id = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
        let hg_cs = hg_cs_id.load(&ctx, rctx.repo().repo_blobstore()).await?;
        let manifest_id = hg_cs.manifestid();

        let hg = rctx.hg();

        let tree = HgTreeContext::new(hg.clone(), manifest_id).await?;
        assert_eq!(manifest_id, tree.node_id());

        let content = tree.content_bytes();

        // The content here is the representation of the format in which
        // the Mercurial client would store a tree manifest node.
        let expected = &b"1\0b8e02f6433738021a065f94175c7cd23db5f05be\nfiles\0b8e02f6433738021a065f94175c7cd23db5f05be\n"[..];
        assert_eq!(content, expected);

        let tree = HgTreeContext::new_check_exists(hg.clone(), manifest_id).await?;
        assert!(tree.is_some());

        let null_id = HgManifestId::new(NULL_HASH);
        let null_tree = HgTreeContext::new(hg.clone(), null_id).await;
        assert!(null_tree.is_err());

        let null_tree = HgTreeContext::new_check_exists(hg.clone(), null_id).await?;
        assert!(null_tree.is_none());

        Ok(())
    }

    // ---- restriction context test helpers ----

    /// Create a CoreContext with a test user identity for ACL checking.
    async fn create_test_ctx(fb: FacebookInit) -> CoreContext {
        let client_identity = MononokeIdentity::from_legacy_type_data("USER", "myusername0");
        let mut cri = ClientRequestInfo::new(ClientEntryPoint::Tests);
        cri.set_main_id("user:myusername0".to_string());
        let client_info = ClientInfo::new_with_client_request_info(cri);

        let identities = BTreeSet::from([client_identity]);
        let metadata = {
            let mut md = Metadata::new(
                Some(&"restricted_paths_test".to_string()),
                identities,
                false,
                false,
                None,
                None,
            )
            .await;
            md.add_client_info(client_info);
            Arc::new(md)
        };
        let session_container = SessionContainer::builder(fb).metadata(metadata).build();
        CoreContext::test_mock_session(session_container)
    }

    /// Create an ACL config where myusername0 has access to myusername_project
    /// but NOT to restricted_acl.
    fn create_test_acls() -> anyhow::Result<Acls> {
        let default_user = MononokeIdentity::from_str("USER:myusername0")?;
        let default_users = {
            let mut users = MononokeIdentitySet::new();
            users.insert(default_user);
            users
        };

        Ok(Acls {
            repos: hashmap! {
                "default".to_string() => Arc::new(Acl {
                    actions: hashmap! {
                        "read".to_string() => default_users.clone(),
                        "write".to_string() => default_users,
                    },
                }),
            },
            repo_regions: hashmap! {
                "myusername_project".to_string() => Arc::new(Acl {
                    actions: hashmap! {
                        "read".to_string() => {
                            let mut users = MononokeIdentitySet::new();
                            users.insert(MononokeIdentity::from_str("USER:myusername0")?);
                            users
                        },
                    },
                }),
                "restricted_acl".to_string() => Arc::new(Acl {
                    actions: hashmap! {
                        "read".to_string() => {
                            let mut users = MononokeIdentitySet::new();
                            users.insert(MononokeIdentity::from_str("USER:another_user")?);
                            users
                        },
                    },
                }),
            },
            tiers: HashMap::new(),
            workspaces: HashMap::new(),
            groups: HashMap::new(),
        })
    }

    /// Build a test repo with restricted paths, real ACL checking, and ManifestIdStore.
    async fn setup_restricted_repo(
        ctx: &CoreContext,
        path_acls: Vec<(&str, &str)>,
    ) -> anyhow::Result<Repo> {
        setup_restricted_repo_with_mode(ctx, path_acls, AclManifestMode::Both, false).await
    }

    async fn setup_restricted_repo_with_mode(
        ctx: &CoreContext,
        path_acls: Vec<(&str, &str)>,
        acl_manifest_mode: AclManifestMode,
        enable_enforcement: bool,
    ) -> anyhow::Result<Repo> {
        let repo_id = RepositoryId::new(0);

        let path_restriction_metadata: HashMap<NonRootMPath, PathRestrictionMetadata> = path_acls
            .into_iter()
            .map(|(path, acl_str)| {
                (
                    NonRootMPath::new(path).expect("Failed to create NonRootMPath"),
                    PathRestrictionMetadata {
                        repo_region_acl: MononokeIdentity::from_str(acl_str)
                            .expect("Failed to parse MononokeIdentity"),
                        permission_request_group: None,
                        rollout_allowlist_group: None,
                        read_only: false,
                    },
                )
            })
            .collect();

        let manifest_id_store = Arc::new(
            SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()
                .expect("Failed to create Sqlite connection")
                .with_repo_id(repo_id),
        );

        let config = RestrictedPathsConfig {
            path_restriction_metadata,
            manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
                use_manifest_id_cache: false,
                cache_update_interval_ms: 5,
                ..Default::default()
            },
            enforcement_condition_sets: enable_enforcement
                .then(|| EnforcementConditionSet {
                    always_enabled: true,
                    require_client_request_flag: false,
                    restriction_acls: Vec::new(),
                    matchers: RequestMatchers::default(),
                })
                .into_iter()
                .collect(),
            enforcement_enabled: enable_enforcement,
            acl_manifest_mode,
            ..Default::default()
        };

        // Write ACLs to temp file for InternalAclProvider
        let acls = create_test_acls()?;
        let mut temp_file = tempfile::NamedTempFile::new()?;
        std::io::Write::write_all(
            &mut temp_file,
            serde_json::to_string_pretty(&acls)?.as_bytes(),
        )?;
        std::io::Write::flush(&mut temp_file)?;
        let acl_path = temp_file.into_temp_path().keep()?;
        let acl_provider = InternalAclProvider::from_file(&acl_path)?;

        let scuba = MononokeScubaSampleBuilder::with_discard();

        // Build a repo first to get ArcRepoDerivedData. Reuse the same
        // factory below so the final test repo shares the same blobstore.
        let mut repo_factory = TestRepoFactory::new(ctx.fb)?;
        let repo: Repo = repo_factory.build().await?;
        let repo_derived_data = repo.repo_derived_data_arc();
        let config_based = Arc::new(RestrictedPathsConfigBased::new(
            config,
            manifest_id_store,
            None,
        ));

        let restricted_paths = Arc::new(RestrictedPaths::new(
            "repo".to_string(),
            config_based,
            acl_provider,
            scuba,
            repo_derived_data,
        )?);

        let repo: Repo = repo_factory
            .with_restricted_paths(restricted_paths)
            .build()
            .await?;

        Ok(repo)
    }

    // ---- restriction context tests ----

    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_no_restrictions_configured(
        fb: FacebookInit,
    ) -> Result<(), MononokeError> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();

        // Use a dummy manifest ID — no restrictions configured, so early return
        let dummy_manifest_id: HgAugmentedManifestId = HgManifestId::new(
            HgNodeHash::from_static_str("0000000000000000000000000000000000000000")?,
        )
        .into();

        let restriction_ctx =
            HgAugmentedTreeRestrictionContext::new_check_exists(hg, dummy_manifest_id).await?;
        let result = restriction_ctx.restriction_check().await?;
        assert!(
            result.is_empty(),
            "expected empty result when no restrictions configured"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_manifest_not_in_store(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let ctx = create_test_ctx(fb).await;
        let repo =
            setup_restricted_repo(&ctx, vec![("restricted/dir", "REPO_REGION:restricted_acl")])
                .await?;

        // Create commit touching only unrestricted paths
        let bcs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("public/file.txt", "content")
            .commit()
            .await?;

        // Derive Hg manifest to populate ManifestIdStore, then load the
        // augmented manifest ID used by restriction_check().
        let _hg_cs_id = repo.derive_hg_changeset(&ctx, bcs_id).await?;
        let (root_mfid, _) = derive_and_load_hg_augmented_manifest(&ctx, &repo, bcs_id).await?;

        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();

        // Root manifest is unrestricted, so not in the ManifestIdStore
        let restriction_ctx =
            HgAugmentedTreeRestrictionContext::new_check_exists(hg, root_mfid).await?;
        let result = restriction_ctx.restriction_check().await?;
        assert!(
            result.is_empty(),
            "expected empty result for unrestricted manifest"
        );

        Ok(())
    }

    /// Helper: create a restricted repo, commit a file, derive Hg manifest,
    /// find the manifest ID for `target_manifest_path`, and call restriction_check().
    async fn get_restriction_check_for_manifest(
        fb: FacebookInit,
        path_acls: Vec<(&str, &str)>,
        file_to_add: (&str, &str),
        target_manifest_path: &str,
    ) -> anyhow::Result<Vec<ManifestRestrictionCheckResult>> {
        let ctx = create_test_ctx(fb).await;
        let repo = setup_restricted_repo(&ctx, path_acls.clone()).await?;

        let mut commit_ctx =
            CreateCommitContext::new_root(&ctx, &repo).add_file(file_to_add.0, file_to_add.1);
        for (root, acl) in path_acls {
            let slacl_path = format!("{root}/.slacl");
            let slacl_content = format!("repo_region_acl = \"{acl}\"\n");
            commit_ctx = commit_ctx.add_file(slacl_path.as_str(), slacl_content);
        }
        let bcs_id = commit_ctx.commit().await?;

        let _hg_cs_id = repo.derive_hg_changeset(&ctx, bcs_id).await?;
        let (_, root_envelope) = derive_and_load_hg_augmented_manifest(&ctx, &repo, bcs_id).await?;

        let target_path = MPath::try_from(target_manifest_path)?;
        let target_mfid =
            load_hg_augmented_manifest_id_at_path(&ctx, &repo, &root_envelope, &target_path)
                .await?;

        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();

        let restriction_ctx =
            HgAugmentedTreeRestrictionContext::new_check_exists(hg, target_mfid).await?;
        restriction_ctx
            .restriction_check()
            .await
            .map_err(Into::into)
    }

    async fn get_both_mode_restriction_check_for_manifest(
        fb: FacebookInit,
        config_path_acls: Vec<(&str, &str)>,
        acl_manifest_path_acls: Vec<(&str, &str)>,
        file_to_add: (&str, &str),
        target_manifest_path: &str,
    ) -> anyhow::Result<Vec<ManifestRestrictionCheckResult>> {
        let (hg, target_mfid) = create_both_mode_manifest_context(
            fb,
            config_path_acls,
            acl_manifest_path_acls,
            file_to_add,
            target_manifest_path,
            false,
        )
        .await?;

        let restriction_ctx =
            HgAugmentedTreeRestrictionContext::new_check_exists(hg, target_mfid).await?;
        restriction_ctx
            .restriction_check()
            .await
            .map_err(Into::into)
    }

    async fn create_both_mode_manifest_context(
        fb: FacebookInit,
        config_path_acls: Vec<(&str, &str)>,
        acl_manifest_path_acls: Vec<(&str, &str)>,
        file_to_add: (&str, &str),
        target_manifest_path: &str,
        enable_enforcement: bool,
    ) -> anyhow::Result<(HgRepoContext<Repo>, HgAugmentedManifestId)> {
        let ctx = create_test_ctx(fb).await;
        let repo = setup_restricted_repo_with_mode(
            &ctx,
            config_path_acls,
            AclManifestMode::Both,
            enable_enforcement,
        )
        .await?;

        let mut commit_ctx =
            CreateCommitContext::new_root(&ctx, &repo).add_file(file_to_add.0, file_to_add.1);
        for (root, acl) in acl_manifest_path_acls {
            let slacl_path = format!("{root}/.slacl");
            let slacl_content = format!("repo_region_acl = \"{acl}\"\n");
            commit_ctx = commit_ctx.add_file(slacl_path.as_str(), slacl_content);
        }
        let bcs_id = commit_ctx.commit().await?;

        let _hg_cs_id = repo.derive_hg_changeset(&ctx, bcs_id).await?;
        let (_, root_envelope) = derive_and_load_hg_augmented_manifest(&ctx, &repo, bcs_id).await?;

        let target_path = MPath::try_from(target_manifest_path)?;
        let target_mfid =
            load_hg_augmented_manifest_id_at_path(&ctx, &repo, &root_envelope, &target_path)
                .await?;

        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();

        Ok((hg, target_mfid))
    }

    // What it tests: Hg manifest restriction metadata should include
    // AclManifest-only restrictions in Both mode after manifest metadata uses
    // the AclManifest source.
    // Expected: AclManifest-only manifest restrictions are returned.
    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_both_mode_acl_manifest_only_root(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let result = get_both_mode_restriction_check_for_manifest(
            fb,
            vec![],
            vec![("acl_manifest_only", "REPO_REGION:restricted_acl")],
            ("acl_manifest_only/file.txt", "secret"),
            "acl_manifest_only",
        )
        .await?;

        let check = result.first().ok_or_else(|| {
            anyhow::anyhow!("expected AclManifest-only restriction info for Both mode")
        })?;
        let info = check.restriction_info();
        assert_eq!(info.restriction_root, None);
        assert_eq!(info.repo_region_acl, "REPO_REGION:restricted_acl");
        assert_eq!(
            check.has_authorization(),
            false,
            "user should not have access to restricted_acl"
        );

        Ok(())
    }

    // What it tests: Hg manifest restriction metadata should still include
    // config-only restrictions in Both mode.
    // Expected: config-backed manifest restrictions are returned.
    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_both_mode_config_only_root(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let result = get_both_mode_restriction_check_for_manifest(
            fb,
            vec![("config_only", "REPO_REGION:restricted_acl")],
            vec![],
            ("config_only/file.txt", "secret"),
            "config_only",
        )
        .await?;

        let check = result.first().ok_or_else(|| {
            anyhow::anyhow!("expected config-only restriction info for Both mode")
        })?;
        let info = check.restriction_info();
        assert_eq!(
            info.restriction_root.as_ref(),
            Some(&NonRootMPath::new("config_only")?)
        );
        assert_eq!(info.repo_region_acl, "REPO_REGION:restricted_acl");

        Ok(())
    }

    // What it tests: Both mode should preserve same-root disagreements between
    // config and AclManifest manifest metadata.
    // Expected: both source results are returned, with config carrying the
    // restriction root and AclManifest carrying only the ACL.
    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_both_mode_same_root_disagreement(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let result = get_both_mode_restriction_check_for_manifest(
            fb,
            vec![("shared", "REPO_REGION:config_acl")],
            vec![("shared", "REPO_REGION:acl_manifest_acl")],
            ("shared/file.txt", "secret"),
            "shared",
        )
        .await?;

        let mut roots_and_acls = result
            .iter()
            .map(|check| {
                let info = check.restriction_info();
                (
                    info.restriction_root.as_ref().map(ToString::to_string),
                    info.repo_region_acl.clone(),
                )
            })
            .collect::<Vec<_>>();
        roots_and_acls.sort();

        assert_eq!(
            roots_and_acls,
            vec![
                (None, "REPO_REGION:acl_manifest_acl".to_string()),
                (
                    Some("shared".to_string()),
                    "REPO_REGION:config_acl".to_string()
                ),
            ]
        );

        Ok(())
    }

    /// What it tests: Both-mode manifest checks retain independent config and
    /// AclManifest authorization factors when their same-root ACLs disagree.
    /// Expected: the rooted config row is denied while the rootless
    /// AclManifest row is authorized directly by its ACL.
    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_both_mode_reports_row_authorization_factors(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let result = get_both_mode_restriction_check_for_manifest(
            fb,
            vec![("shared", "REPO_REGION:restricted_acl")],
            vec![("shared", "REPO_REGION:myusername_project")],
            ("shared/file.txt", "secret"),
            "shared",
        )
        .await?;

        let mut rows = result
            .iter()
            .map(|check| {
                let info = check.restriction_info();
                (
                    info.restriction_root.as_ref().map(ToString::to_string),
                    info.repo_region_acl.clone(),
                    check.has_authorization(),
                    check.has_acl_access(),
                    check.is_rollout_allowlisted(),
                    check.is_allowlisted_tooling(),
                    check.is_admin_bypass(),
                )
            })
            .collect::<Vec<_>>();
        rows.sort();

        assert_eq!(
            rows,
            vec![
                (
                    None,
                    "REPO_REGION:myusername_project".to_string(),
                    true,
                    true,
                    false,
                    false,
                    false,
                ),
                (
                    Some("shared".to_string()),
                    "REPO_REGION:restricted_acl".to_string(),
                    false,
                    false,
                    false,
                    false,
                    false,
                ),
            ]
        );

        Ok(())
    }

    /// What it tests: the Hg augmented-tree constructor propagates Both-mode
    /// enforcement when config denies and AclManifest allows the same root.
    /// Expected: constructing the restricted tree returns a restricted-paths
    /// authorization error rather than serving the manifest.
    #[mononoke::fbinit_test]
    async fn test_hg_augmented_tree_context_both_mode_propagates_denial(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let (hg, target_mfid) = create_both_mode_manifest_context(
            fb,
            vec![("shared", "REPO_REGION:restricted_acl")],
            vec![("shared", "REPO_REGION:myusername_project")],
            ("shared/file.txt", "secret"),
            "shared",
            true,
        )
        .await?;

        let result = HgAugmentedTreeContext::new_check_exists(hg, target_mfid).await;
        let Err(error) = result else {
            anyhow::bail!("expected Both-mode manifest enforcement to deny access");
        };
        assert!(
            matches!(error, MononokeError::RestrictedPathsAuthorizationError(_)),
            "expected restricted-paths authorization error, got {error:#}"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_restricted_path_with_access(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let result = get_restriction_check_for_manifest(
            fb,
            vec![("user_project/foo", "REPO_REGION:myusername_project")],
            ("user_project/foo/bar/a", "content"),
            "user_project/foo",
        )
        .await?;

        let check = result
            .iter()
            .find(|check| check.restriction_info().restriction_root.is_none())
            .ok_or_else(|| anyhow::anyhow!("expected AclManifest restriction info"))?;
        let info = check.restriction_info();
        assert_eq!(
            info.restriction_root, None,
            "AclManifest-backed manifest metadata does not include the path root"
        );
        assert_eq!(
            check.has_authorization(),
            true,
            "user should have access to myusername_project"
        );
        assert_eq!(info.repo_region_acl, "REPO_REGION:myusername_project");

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_restricted_path_without_access(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let result = get_restriction_check_for_manifest(
            fb,
            vec![("restricted/dir", "REPO_REGION:restricted_acl")],
            ("restricted/dir/a", "secret"),
            "restricted/dir",
        )
        .await?;

        let check = result
            .iter()
            .find(|check| check.restriction_info().restriction_root.is_none())
            .ok_or_else(|| anyhow::anyhow!("expected AclManifest restriction info"))?;
        let info = check.restriction_info();
        assert_eq!(
            info.restriction_root, None,
            "AclManifest-backed manifest metadata does not include the path root"
        );
        assert_eq!(
            check.has_authorization(),
            false,
            "user should not have access to restricted_acl"
        );
        assert_eq!(info.repo_region_acl, "REPO_REGION:restricted_acl");

        Ok(())
    }

    /// When nested restriction roots exist (e.g. `foo/` and `foo/bar/`),
    /// querying the manifest at the inner root should return the inner root's
    /// ACL — matching AclManifest behavior where each directory has exactly
    /// one ACL.
    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_nested_roots(fb: FacebookInit) -> anyhow::Result<()> {
        // Query the manifest at foo/bar, which is covered by both roots.
        // restriction_check should return foo/bar's ACL (the most specific).
        let result = get_restriction_check_for_manifest(
            fb,
            vec![
                ("foo", "REPO_REGION:myusername_project"),
                ("foo/bar", "REPO_REGION:restricted_acl"),
            ],
            ("foo/bar/qux/file.txt", "content"),
            "foo/bar",
        )
        .await?;

        let check = result
            .iter()
            .find(|check| check.restriction_info().restriction_root.is_none())
            .ok_or_else(|| anyhow::anyhow!("expected nested AclManifest restriction info"))?;
        let info = check.restriction_info();
        assert_eq!(
            info.restriction_root, None,
            "AclManifest-backed manifest metadata does not include the path root"
        );
        assert_eq!(
            check.has_authorization(),
            false,
            "user should not have access to restricted_acl"
        );
        assert_eq!(info.repo_region_acl, "REPO_REGION:restricted_acl");

        Ok(())
    }

    /// Edge case: When two directories with identical content are both
    /// restriction roots, they share a single Hg manifest ID.
    /// In the long-term solution, this will only happen if they have the same
    /// ACL file, which means they also share `PathAccessInfo`.
    #[mononoke::fbinit_test]
    async fn test_check_manifest_permission_same_manifest_multiple_paths(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let ctx = create_test_ctx(fb).await;
        let repo = setup_restricted_repo(
            &ctx,
            vec![
                ("dir_a", "REPO_REGION:restricted_acl"),
                ("dir_b", "REPO_REGION:restricted_acl"),
            ],
        )
        .await?;

        // Create two directories with identical content and identical ACL files.
        // Identical content produces the same Hg augmented manifest ID.
        let bcs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir_a/file.txt", "same content")
            .add_file(
                "dir_a/.slacl",
                "repo_region_acl = \"REPO_REGION:restricted_acl\"\n",
            )
            .add_file("dir_b/file.txt", "same content")
            .add_file(
                "dir_b/.slacl",
                "repo_region_acl = \"REPO_REGION:restricted_acl\"\n",
            )
            .commit()
            .await?;

        let _hg_cs_id = repo.derive_hg_changeset(&ctx, bcs_id).await?;
        let (_, root_envelope) = derive_and_load_hg_augmented_manifest(&ctx, &repo, bcs_id).await?;

        let dir_a = MPath::try_from("dir_a")?;
        let dir_a_mfid =
            load_hg_augmented_manifest_id_at_path(&ctx, &repo, &root_envelope, &dir_a).await?;

        let dir_b = MPath::try_from("dir_b")?;
        let dir_b_mfid =
            load_hg_augmented_manifest_id_at_path(&ctx, &repo, &root_envelope, &dir_b).await?;

        // Verify they share the same manifest ID (identical content)
        assert_eq!(
            dir_a_mfid, dir_b_mfid,
            "identical directories should produce the same manifest ID"
        );

        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();

        // Query the shared manifest ID. The AclManifest metadata for both roots
        // is identical, so the check should return that shared ACL.
        let restriction_ctx =
            HgAugmentedTreeRestrictionContext::new_check_exists(hg, dir_a_mfid).await?;
        let result = restriction_ctx.restriction_check().await?;

        let check = result.first().ok_or_else(|| {
            anyhow::anyhow!("expected restriction info when manifest maps to restricted root paths")
        })?;
        let info = check.restriction_info();
        assert_eq!(
            check.has_authorization(),
            false,
            "user should not have access to restricted_acl"
        );
        assert_eq!(info.repo_region_acl, "REPO_REGION:restricted_acl");

        Ok(())
    }

    async fn derive_and_load_hg_augmented_manifest(
        ctx: &CoreContext,
        repo: &Repo,
        bcs_id: ChangesetId,
    ) -> anyhow::Result<(HgAugmentedManifestId, HgAugmentedManifestEnvelope)> {
        let manager = repo.repo_derived_data().manager();
        manager
            .derive_exactly_batch::<MappedHgChangesetId>(ctx, vec![bcs_id], None)
            .await?;
        manager
            .derive_exactly_batch::<RootAclManifestId>(ctx, vec![bcs_id], None)
            .await?;
        manager
            .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, vec![bcs_id], None)
            .await?;

        let root_hg_augmented_manifest_id = manager
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, bcs_id, None)
            .await?
            .with_context(|| format!("Missing RootHgAugmentedManifestId for {bcs_id}"))?
            .hg_augmented_manifest_id();
        let root_envelope = root_hg_augmented_manifest_id
            .clone()
            .load(ctx, repo.repo_blobstore())
            .await?;
        Ok((root_hg_augmented_manifest_id, root_envelope))
    }

    async fn load_hg_augmented_manifest_id_at_path(
        ctx: &CoreContext,
        repo: &Repo,
        root_envelope: &HgAugmentedManifestEnvelope,
        path: &MPath,
    ) -> anyhow::Result<HgAugmentedManifestId> {
        let elements: Vec<_> = path.into_iter().collect();
        anyhow::ensure!(!elements.is_empty(), "path must have at least one segment");

        let mut current_envelope = Cow::Borrowed(root_envelope);
        for (index, element) in elements.iter().enumerate() {
            let entry = current_envelope
                .augmented_manifest
                .subentries
                .lookup(ctx, repo.repo_blobstore(), element.as_ref())
                .await?
                .with_context(|| format!("{element} should exist at depth {index}"))?;
            match entry {
                HgAugmentedManifestEntry::DirectoryNode(directory) => {
                    let hg_augmented_manifest_id = HgAugmentedManifestId::new(directory.treenode);
                    if index == elements.len() - 1 {
                        return Ok(hg_augmented_manifest_id);
                    }
                    current_envelope = Cow::Owned(
                        hg_augmented_manifest_id
                            .load(ctx, repo.repo_blobstore())
                            .await?,
                    );
                }
                HgAugmentedManifestEntry::FileNode(_) => {
                    anyhow::bail!("{element} should be a directory node");
                }
            }
        }
        unreachable!("non-empty path traversal returns from the final element")
    }
}
