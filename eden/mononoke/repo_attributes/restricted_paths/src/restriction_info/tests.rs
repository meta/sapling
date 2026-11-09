/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestEnvelope;
use mercurial_types::HgAugmentedManifestId;
use metaconfig_types::AclManifestMode;
use metaconfig_types::PathRestrictionMetadata;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use permission_checker::dummy::DummyAclProvider;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstruct;
use tests_utils::CreateCommitContext;

use super::ManifestRestrictionInfo;
use super::PathRestrictionInfo;
use super::find_restricted_descendants_from_acl_manifest;
use super::get_manifest_restriction_info_from_acl_manifest;
use super::get_path_restriction_info_from_acl_manifest;
use super::get_path_restriction_root_info_from_acl_manifest;
use super::is_restricted_manifest;
use super::union_manifest_restriction_info_with_acl_manifest_precedence;
use super::union_path_restriction_info_with_acl_manifest_precedence;
use crate::ManifestType;
use crate::RestrictedManifestId;
use crate::RestrictedPathManifestIdEntry;
use crate::RestrictedPaths;
use crate::RestrictedPathsConfig;
use crate::RestrictedPathsConfigBased;
use crate::SqlRestrictedPathsManifestIdStoreBuilder;

#[facet::container]
struct AclManifestLookupTestRepo(
    dyn BonsaiHgMapping,
    dyn Bookmarks,
    CommitGraph,
    dyn CommitGraphWriter,
    RepoDerivedData,
    RepoBlobstore,
    FilestoreConfig,
    RepoIdentity,
);

struct AclManifestLookupFixture {
    ctx: CoreContext,
    restricted_paths: RestrictedPaths,
    cs_id: ChangesetId,
}

struct ManifestLookupFixture {
    ctx: CoreContext,
    restricted_paths: RestrictedPaths,
    manifest_id: RestrictedManifestId,
    manifest_type: ManifestType,
}

const CONFIG_ACL: &str = "REPO_REGION:repos/hg/fbsource/=configured";

mod manifest_metadata_union {
    use super::*;

    #[mononoke::test]
    fn test_preserves_rootless_acl_manifest_entries() -> Result<()> {
        let first = manifest_restriction_info(None, "REPO_REGION:first")?;
        let second = manifest_restriction_info(None, "REPO_REGION:second")?;

        let results = union_manifest_restriction_info_with_acl_manifest_precedence(
            vec![],
            vec![first.clone(), second.clone()],
        );

        assert_eq!(results, vec![first, second]);
        Ok(())
    }

    #[mononoke::test]
    fn test_acl_manifest_wins_for_duplicate_known_root() -> Result<()> {
        let config = manifest_restriction_info(Some("shared"), "REPO_REGION:config")?;
        let acl_manifest = manifest_restriction_info(Some("shared"), "REPO_REGION:acl_manifest")?;

        let results = union_manifest_restriction_info_with_acl_manifest_precedence(
            vec![config],
            vec![acl_manifest.clone()],
        );

        assert_eq!(results, vec![acl_manifest]);
        Ok(())
    }
}

mod path_metadata_union {
    use super::*;

    #[mononoke::test]
    fn test_acl_manifest_wins_for_duplicate_root() -> Result<()> {
        let root = NonRootMPath::new("shared")?;
        let config = PathRestrictionInfo {
            restriction_root: root.clone(),
            repo_region_acl: "REPO_REGION:config".to_string(),
            permission_request_group: "REPO_REGION:config".parse()?,
            rollout_allowlist_group: None,
        };
        let acl_manifest = PathRestrictionInfo {
            restriction_root: root,
            repo_region_acl: "REPO_REGION:acl_manifest".to_string(),
            permission_request_group: "REPO_REGION:acl_manifest".parse()?,
            rollout_allowlist_group: None,
        };

        let results = union_path_restriction_info_with_acl_manifest_precedence(
            vec![config],
            vec![acl_manifest.clone()],
        );

        assert_eq!(
            results,
            vec![acl_manifest],
            "AclManifest wins over config for a duplicate restriction root"
        );
        Ok(())
    }
}

mod acl_manifest_path_lookup {
    use super::*;

    #[mononoke::fbinit_test]
    async fn test_exact_path_lookup_finds_restriction_root(fb: FacebookInit) -> Result<()> {
        let restriction_root = "restricted";
        let repo_region_acl = "REPO_REGION:repos/hg/fbsource/=restricted";
        let fixture = acl_manifest_lookup_fixture(
            fb,
            vec![
                ("restricted/.slacl", slacl(repo_region_acl)),
                ("restricted/file.txt", b"content".to_vec()),
            ],
        )
        .await?;
        let results = get_path_restriction_root_info_from_acl_manifest(
            &fixture.restricted_paths,
            &fixture.ctx,
            fixture.cs_id,
            &[NonRootMPath::new(restriction_root)?],
        )
        .await?;

        assert_eq!(
            results,
            vec![PathRestrictionInfo {
                restriction_root: NonRootMPath::new(restriction_root)?,
                repo_region_acl: repo_region_acl.to_string(),
                permission_request_group: repo_region_acl.parse()?,
                rollout_allowlist_group: None,
            }],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_ancestor_path_lookup_finds_parent_restriction(fb: FacebookInit) -> Result<()> {
        let restriction_root = "restricted";
        let lookup_path = "restricted/child/file.txt";
        let repo_region_acl = "REPO_REGION:repos/hg/fbsource/=restricted_parent";
        let fixture = acl_manifest_lookup_fixture(
            fb,
            vec![
                ("restricted/.slacl", slacl(repo_region_acl)),
                (lookup_path, b"content".to_vec()),
            ],
        )
        .await?;
        let results = get_path_restriction_info_from_acl_manifest(
            &fixture.restricted_paths,
            &fixture.ctx,
            fixture.cs_id,
            &[NonRootMPath::new(lookup_path)?],
        )
        .await?;

        assert_eq!(
            results,
            vec![PathRestrictionInfo {
                restriction_root: NonRootMPath::new(restriction_root)?,
                repo_region_acl: repo_region_acl.to_string(),
                permission_request_group: repo_region_acl.parse()?,
                rollout_allowlist_group: None,
            }],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_descendant_lookup_finds_restricted_children(fb: FacebookInit) -> Result<()> {
        let lookup_root = "project";
        let first_restriction_root = "project/first";
        let first_repo_region_acl = "REPO_REGION:repos/hg/fbsource/=first";
        let second_restriction_root = "project/second";
        let second_repo_region_acl = "REPO_REGION:repos/hg/fbsource/=second";
        let fixture = acl_manifest_lookup_fixture(
            fb,
            vec![
                ("project/first/.slacl", slacl(first_repo_region_acl)),
                ("project/first/file.txt", b"content".to_vec()),
                ("project/second/.slacl", slacl(second_repo_region_acl)),
                ("project/second/file.txt", b"content".to_vec()),
            ],
        )
        .await?;
        let results = find_restricted_descendants_from_acl_manifest(
            &fixture.restricted_paths,
            &fixture.ctx,
            fixture.cs_id,
            vec![MPath::from(NonRootMPath::new(lookup_root)?)],
        )
        .await?;

        assert_eq!(
            results,
            vec![
                PathRestrictionInfo {
                    restriction_root: NonRootMPath::new(first_restriction_root)?,
                    repo_region_acl: first_repo_region_acl.to_string(),
                    permission_request_group: first_repo_region_acl.parse()?,
                    rollout_allowlist_group: None,
                },
                PathRestrictionInfo {
                    restriction_root: NonRootMPath::new(second_restriction_root)?,
                    repo_region_acl: second_repo_region_acl.to_string(),
                    permission_request_group: second_repo_region_acl.parse()?,
                    rollout_allowlist_group: None,
                },
            ],
        );
        Ok(())
    }
}

mod hg_augmented_manifest_lookup {
    use super::*;

    #[mononoke::fbinit_test]
    async fn test_hg_augmented_manifest_lookup_reads_acl_manifest_directory_id(
        fb: FacebookInit,
    ) -> Result<()> {
        let restriction_root = "manifest_restricted";
        let repo_region_acl = "REPO_REGION:repos/hg/fbsource/=manifest_restricted";
        let permission_request_group = "GROUP:manifest_restricted_requests";
        let fixture = hg_augmented_manifest_lookup_fixture(
            fb,
            vec![
                (
                    "manifest_restricted/.slacl",
                    slacl_with_permission_request_group(repo_region_acl, permission_request_group),
                ),
                ("manifest_restricted/file.txt", b"content".to_vec()),
            ],
            Some(restriction_root),
            ManifestType::HgAugmented,
        )
        .await?;
        let results = get_manifest_restriction_info_from_acl_manifest(
            &fixture.restricted_paths,
            &fixture.ctx,
            &fixture.manifest_id,
            &fixture.manifest_type,
        )
        .await?;

        assert_eq!(
            results,
            vec![ManifestRestrictionInfo {
                restriction_root: None,
                repo_region_acl: repo_region_acl.to_string(),
                permission_request_group: permission_request_group.parse()?,
                rollout_allowlist_group: None,
            }],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_hg_augmented_manifest_lookup_skips_unrestricted_manifest(
        fb: FacebookInit,
    ) -> Result<()> {
        let fixture = hg_augmented_manifest_lookup_fixture(
            fb,
            vec![("unrestricted/file.txt", b"content".to_vec())],
            None,
            ManifestType::HgAugmented,
        )
        .await?;
        let results = get_manifest_restriction_info_from_acl_manifest(
            &fixture.restricted_paths,
            &fixture.ctx,
            &fixture.manifest_id,
            &fixture.manifest_type,
        )
        .await?;

        assert_eq!(results, vec![]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_hg_augmented_manifest_lookup_errors_for_unsupported_manifest_type(
        fb: FacebookInit,
    ) -> Result<()> {
        let fixture = hg_augmented_manifest_lookup_fixture(
            fb,
            vec![("unsupported/file.txt", b"content".to_vec())],
            None,
            ManifestType::Hg,
        )
        .await?;
        let results = get_manifest_restriction_info_from_acl_manifest(
            &fixture.restricted_paths,
            &fixture.ctx,
            &fixture.manifest_id,
            &fixture.manifest_type,
        )
        .await;

        let err = results
            .err()
            .context("expected unsupported manifest type to error")?;

        assert!(
            err.to_string()
                .contains("AclManifest manifest restriction lookup only supports HgAugmented"),
            "unexpected error: {err:#}"
        );

        Ok(())
    }
}

mod manifest_restriction_metadata {
    use super::*;

    #[mononoke::fbinit_test]
    async fn test_rejects_unsupported_manifest_type(fb: FacebookInit) -> Result<()> {
        let (ctx, restricted_paths) =
            manifest_restriction_metadata_fixture(fb, AclManifestMode::Both).await?;

        let err = is_restricted_manifest(
            &restricted_paths,
            &ctx,
            &RestrictedManifestId::from("unsupported_manifest"),
            &ManifestType::Hg,
            false,
        )
        .await
        .err()
        .context("expected unsupported manifest type to error")?;

        assert!(
            err.to_string()
                .contains("AclManifest manifest restriction lookup only supports HgAugmented"),
            "unexpected error: {err:#}"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_authoritative_uses_preloaded_acl_manifest_value(fb: FacebookInit) -> Result<()> {
        let (ctx, restricted_paths) =
            manifest_restriction_metadata_fixture(fb, AclManifestMode::Authoritative).await?;
        let manifest_id = RestrictedManifestId::from("authoritative_manifest");
        add_manifest_id_store_entry(&restricted_paths, &ctx, manifest_id.clone()).await?;

        assert!(
            !is_restricted_manifest(
                &restricted_paths,
                &ctx,
                &manifest_id,
                &ManifestType::HgAugmented,
                false,
            )
            .await?
        );
        assert!(
            is_restricted_manifest(
                &restricted_paths,
                &ctx,
                &manifest_id,
                &ManifestType::HgAugmented,
                true,
            )
            .await?
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_both_unions_preloaded_acl_manifest_and_config_store(
        fb: FacebookInit,
    ) -> Result<()> {
        let (ctx, restricted_paths) =
            manifest_restriction_metadata_fixture(fb, AclManifestMode::Both).await?;
        let manifest_id = RestrictedManifestId::from("both_manifest");

        assert!(
            is_restricted_manifest(
                &restricted_paths,
                &ctx,
                &manifest_id,
                &ManifestType::HgAugmented,
                true,
            )
            .await?
        );
        assert!(
            !is_restricted_manifest(
                &restricted_paths,
                &ctx,
                &manifest_id,
                &ManifestType::HgAugmented,
                false,
            )
            .await?
        );

        add_manifest_id_store_entry(&restricted_paths, &ctx, manifest_id.clone()).await?;

        assert!(
            is_restricted_manifest(
                &restricted_paths,
                &ctx,
                &manifest_id,
                &ManifestType::HgAugmented,
                false,
            )
            .await?
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_config_backed_modes_use_manifest_id_store_without_current_config_revalidation(
        fb: FacebookInit,
    ) -> Result<()> {
        for (mode, manifest_id) in [
            (
                AclManifestMode::Disabled,
                RestrictedManifestId::from("disabled_manifest"),
            ),
            (
                AclManifestMode::Shadow,
                RestrictedManifestId::from("shadow_manifest"),
            ),
        ] {
            let (ctx, restricted_paths) = manifest_restriction_metadata_fixture(fb, mode).await?;
            add_manifest_id_store_entry(&restricted_paths, &ctx, manifest_id.clone()).await?;

            assert!(
                is_restricted_manifest(
                    &restricted_paths,
                    &ctx,
                    &manifest_id,
                    &ManifestType::HgAugmented,
                    false,
                )
                .await?
            );
        }

        Ok(())
    }
}

async fn acl_manifest_lookup_fixture(
    fb: FacebookInit,
    files: Vec<(&'static str, Vec<u8>)>,
) -> Result<AclManifestLookupFixture> {
    let ctx = CoreContext::test_mock(fb);
    let repo: AclManifestLookupTestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = files
        .into_iter()
        .fold(
            CreateCommitContext::new_root(&ctx, &repo),
            |commit, (path, content)| commit.add_file(path, content),
        )
        .commit()
        .await?;
    let restricted_paths = restricted_paths_for_repo(fb, &repo)?;

    Ok(AclManifestLookupFixture {
        ctx,
        restricted_paths,
        cs_id,
    })
}

async fn hg_augmented_manifest_lookup_fixture(
    fb: FacebookInit,
    files: Vec<(&'static str, Vec<u8>)>,
    manifest_path: Option<&'static str>,
    manifest_type: ManifestType,
) -> Result<ManifestLookupFixture> {
    let ctx = CoreContext::test_mock(fb);
    let repo: AclManifestLookupTestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = files
        .into_iter()
        .fold(
            CreateCommitContext::new_root(&ctx, &repo),
            |commit, (path, content)| commit.add_file(path, content),
        )
        .commit()
        .await?;
    let (root_manifest_id, root_envelope) =
        derive_and_load_hg_augmented_manifest(&ctx, &repo, cs_id).await?;
    let hg_augmented_manifest_id = match manifest_path {
        Some(path) => {
            load_hg_augmented_manifest_id_at_path(&ctx, &repo, &root_envelope, &MPath::new(path)?)
                .await?
        }
        None => root_manifest_id,
    };
    let restricted_paths = restricted_paths_for_repo(fb, &repo)?;

    Ok(ManifestLookupFixture {
        ctx,
        restricted_paths,
        manifest_id: manifest_id_from_hg_augmented_id(hg_augmented_manifest_id),
        manifest_type,
    })
}

fn restricted_paths_for_repo(
    fb: FacebookInit,
    repo: &AclManifestLookupTestRepo,
) -> Result<RestrictedPaths> {
    restricted_paths_for_repo_with_mode_and_path_acls(fb, repo, AclManifestMode::Both, vec![])
}

fn restricted_paths_for_repo_with_mode_and_path_acls(
    fb: FacebookInit,
    repo: &AclManifestLookupTestRepo,
    acl_manifest_mode: AclManifestMode,
    path_acls: Vec<(&str, &str)>,
) -> Result<RestrictedPaths> {
    let manifest_id_store = Arc::new(
        SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?
            .with_repo_id(RepositoryId::new(0)),
    );
    let path_restriction_metadata = path_acls
        .into_iter()
        .map(|(path, acl)| {
            Ok((
                NonRootMPath::new(path)?,
                PathRestrictionMetadata {
                    repo_region_acl: acl.parse()?,
                    permission_request_group: None,
                    rollout_allowlist_group: None,
                    read_only: false,
                },
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let config_based = Arc::new(RestrictedPathsConfigBased::new(
        RestrictedPathsConfig {
            acl_manifest_mode,
            path_restriction_metadata,
            ..Default::default()
        },
        manifest_id_store,
        None,
    ));
    RestrictedPaths::new(
        "repo".to_string(),
        config_based,
        DummyAclProvider::new(fb)?,
        MononokeScubaSampleBuilder::with_discard(),
        repo.repo_derived_data_arc(),
    )
}

async fn manifest_restriction_metadata_fixture(
    fb: FacebookInit,
    acl_manifest_mode: AclManifestMode,
) -> Result<(CoreContext, RestrictedPaths)> {
    let ctx = CoreContext::test_mock(fb);
    let repo: AclManifestLookupTestRepo = test_repo_factory::build_empty(fb).await?;
    let restricted_paths = restricted_paths_for_repo_with_mode_and_path_acls(
        fb,
        &repo,
        acl_manifest_mode,
        vec![("still_configured", CONFIG_ACL)],
    )?;
    Ok((ctx, restricted_paths))
}

async fn add_manifest_id_store_entry(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: RestrictedManifestId,
) -> Result<()> {
    restricted_paths
        .config_based()
        .manifest_id_store()
        .add_entry(
            ctx,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                manifest_id,
                RepoPath::DirectoryPath(NonRootMPath::new("removed_from_config")?),
            )?,
        )
        .await?;
    Ok(())
}

async fn derive_and_load_hg_augmented_manifest(
    ctx: &CoreContext,
    repo: &AclManifestLookupTestRepo,
    cs_id: ChangesetId,
) -> Result<(HgAugmentedManifestId, HgAugmentedManifestEnvelope)> {
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(ctx, vec![cs_id], None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(ctx, vec![cs_id], None)
        .await?;
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, vec![cs_id], None)
        .await?;
    let root_hg_augmented_manifest_id = manager
        .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
        .await?
        .with_context(|| format!("Missing RootHgAugmentedManifestId for {cs_id}"))?
        .hg_augmented_manifest_id();
    let root_envelope = root_hg_augmented_manifest_id
        .clone()
        .load(ctx, repo.repo_blobstore())
        .await?;
    Ok((root_hg_augmented_manifest_id, root_envelope))
}

async fn load_hg_augmented_manifest_id_at_path(
    ctx: &CoreContext,
    repo: &AclManifestLookupTestRepo,
    root_envelope: &HgAugmentedManifestEnvelope,
    path: &MPath,
) -> Result<HgAugmentedManifestId> {
    let elements: Vec<_> = path.into_iter().collect();
    anyhow::ensure!(!elements.is_empty(), "path must have at least one segment");

    let mut current_envelope = std::borrow::Cow::Borrowed(root_envelope);
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
                let child = hg_augmented_manifest_id
                    .load(ctx, repo.repo_blobstore())
                    .await?;
                current_envelope = std::borrow::Cow::Owned(child);
            }
            HgAugmentedManifestEntry::FileNode(_) => {
                anyhow::bail!("{element} should be a directory node");
            }
        }
    }
    anyhow::bail!("path must have at least one segment")
}

fn manifest_id_from_hg_augmented_id(
    hg_augmented_manifest_id: HgAugmentedManifestId,
) -> RestrictedManifestId {
    hg_augmented_manifest_id.to_string().into()
}

fn slacl(repo_region_acl: &str) -> Vec<u8> {
    format!("repo_region_acl = \"{repo_region_acl}\"\n").into_bytes()
}

fn slacl_with_permission_request_group(
    repo_region_acl: &str,
    permission_request_group: &str,
) -> Vec<u8> {
    format!(
        "repo_region_acl = \"{repo_region_acl}\"\npermission_request_group = \"{permission_request_group}\"\n"
    )
    .into_bytes()
}

fn manifest_restriction_info(
    restriction_root: Option<&str>,
    repo_region_acl: &str,
) -> Result<ManifestRestrictionInfo> {
    Ok(ManifestRestrictionInfo {
        restriction_root: restriction_root.map(NonRootMPath::new).transpose()?,
        repo_region_acl: repo_region_acl.to_string(),
        permission_request_group: repo_region_acl.parse()?,
        rollout_allowlist_group: None,
    })
}
