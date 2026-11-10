/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Result;
    use ascii::AsciiString;
    use blobstore::Loadable;
    use bookmarks::BookmarkKey;
    use bookmarks::BookmarkUpdateLogRef;
    use bookmarks::BookmarkUpdateReason;
    use bookmarks::BookmarksRef;
    use bookmarks::Freshness;
    use bulk_derivation::BulkDerivation;
    use cached_config::ConfigStore;
    use cached_config::ModificationTime;
    use cached_config::TestSource;
    use context::CoreContext;
    use cross_repo_sync::CommitSyncContext;
    use cross_repo_sync::SubmoduleDeps;
    use cross_repo_sync::create_commit_syncers;
    use cross_repo_sync::unsafe_always_rewrite_sync_commit;
    use derivation_queue_thrift::DerivationPriority;
    use derived_data_manager::BonsaiDerivable;
    use fbinit::FacebookInit;
    use futures::FutureExt;
    use futures::stream::TryStreamExt;
    use git_types::MappedGitCommitId;
    use git_types::RootGitDeltaManifestV2Id;
    use git_types::RootGitDeltaManifestV3Id;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use live_commit_sync_config::CONFIGERATOR_ALL_COMMIT_SYNC_CONFIGS;
    use live_commit_sync_config::CfgrLiveCommitSyncConfig;
    use live_commit_sync_config::LiveCommitSyncConfig;
    use live_commit_sync_config::TestLiveCommitSyncConfig;
    use maplit::hashmap;
    use memblob::Memblob;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mercurial_types::NonRootMPath;
    use metaconfig_parser::RepoConfigs;
    use metaconfig_types::CommitSyncConfig;
    use metaconfig_types::CommitSyncConfigVersion;
    use metaconfig_types::CommonCommitSyncConfig;
    use metaconfig_types::CommonConfig;
    use metaconfig_types::DefaultSmallToLargeCommitSyncPathAction;
    use metaconfig_types::GlobalrevConfig;
    use metaconfig_types::PushrebaseParams;
    use metaconfig_types::RepoConfig;
    use metaconfig_types::SmallRepoCommitSyncConfig;
    use metaconfig_types::SmallRepoPermanentConfig;
    use mononoke_macros::mononoke;
    use mononoke_types::BonsaiChangeset;
    use mononoke_types::ChangesetId;
    use mononoke_types::DateTime;
    use mononoke_types::RepositoryId;
    use mononoke_types::globalrev::Globalrev;
    use mononoke_types::globalrev::START_COMMIT_GLOBALREV;
    use movers::CrossRepoMover;
    use movers::DefaultAction;
    use movers::Mover;
    use movers::Movers;
    use pushredirect::PushRedirectionConfig;
    use pushredirect::TestPushRedirectionConfig;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedDataRef;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;
    use tests_utils::drawdag::create_from_dag;
    use tests_utils::hg_manifest_reads::HgManifestReadBlocker;
    use tests_utils::list_working_copy_utf8;

    use crate::ChangesetArgs;
    use crate::CheckerFlags;
    use crate::CombinedMover;
    use crate::ImportStage;
    use crate::InvalidReverseMover;
    use crate::RecoveryFields;
    use crate::Repo;
    use crate::RepoImportSetting;
    use crate::back_sync_commits_to_small_repo;
    use crate::derive_bonsais_single_repo;
    use crate::find_mapping_version;
    use crate::get_large_repo_config_if_pushredirected;
    use crate::get_large_repo_setting;
    use crate::get_leaf_entries;
    use crate::merge_imported_commit;
    use crate::move_bookmark;
    use crate::push_merge_commit;
    use crate::rewrite_file_paths;

    fn create_bookmark_name(book: &str) -> BookmarkKey {
        BookmarkKey::new(book).unwrap()
    }

    fn mp(s: &'static str) -> NonRootMPath {
        NonRootMPath::new(s).unwrap()
    }

    async fn create_repo(fb: FacebookInit, id: i32) -> Result<Repo> {
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_config_override(|config| {
                let config = config
                    .derived_data_config
                    .get_active_config_mut()
                    .expect("No enabled derived data types config");
                // Repo import has no need of these derived data types
                config.types.remove(&MappedGitCommitId::VARIANT);
                config.types.remove(&RootGitDeltaManifestV2Id::VARIANT);
                config.types.remove(&RootGitDeltaManifestV3Id::VARIANT);
            })
            .with_id(RepositoryId::new(id))
            .build()
            .await?;
        Ok(repo)
    }

    async fn create_repo_with_live_commit_sync_config(
        fb: FacebookInit,
        id: i32,
        live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    ) -> Result<Repo> {
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_config_override(|config| {
                let config = config
                    .derived_data_config
                    .get_active_config_mut()
                    .expect("No enabled derived data types config");
                // Repo import has no need of these derived data types
                config.types.remove(&MappedGitCommitId::VARIANT);
                config.types.remove(&RootGitDeltaManifestV2Id::VARIANT);
                config.types.remove(&RootGitDeltaManifestV3Id::VARIANT);
            })
            .with_live_commit_sync_config(live_commit_sync_config.clone())
            .with_id(RepositoryId::new(id))
            .build()
            .await?;
        Ok(repo)
    }

    fn get_file_changes_mpaths(bcs: &BonsaiChangeset) -> Vec<NonRootMPath> {
        bcs.file_changes()
            .map(|(mpath, _)| mpath)
            .cloned()
            .collect()
    }

    fn create_mock_recovery_fields() -> RecoveryFields {
        RecoveryFields {
            import_stage: ImportStage::GitImport,
            recovery_file_path: "recovery_path".to_string(),
            git_repo_path: "git_repo_path".to_string(),
            git_merge_bcs_id: None,
            git_merge_rev_id: "master".to_string(),
            dest_path: "dest_path".to_string(),
            bookmark_suffix: "bookmark_suffix".to_string(),
            batch_size: 2,
            move_bookmark_commits_done: 0,
            phab_check_disabled: true,
            x_repo_check_disabled: true,
            sleep_time: Duration::from_secs(1),
            dest_bookmark_name: "dest_bookmark_name".to_string(),
            commit_author: "commit_author".to_string(),
            commit_message: "commit_message".to_string(),
            datetime: DateTime::now(),
            mark_not_synced_mapping: None,
            imported_cs_id: None,
            shifted_bcs_ids: None,
            gitimport_bcs_ids: None,
            merged_cs_id: None,
            print_gitimport_map: false,
        }
    }

    #[mononoke::fbinit_test]
    async fn test_move_bookmark(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
        let mut recovery_fields = create_mock_recovery_fields();
        let call_sign = Some("FBS".to_string());
        let checker_flags = CheckerFlags {
            phab_check_disabled: true,
            x_repo_check_disabled: true,
        };
        let changesets = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C-D-E-F-G
            "##,
        )
        .await?;

        let bcs_ids: Vec<ChangesetId> = changesets.values().copied().collect();
        let importing_bookmark = BookmarkKey::new("repo_import_test_repo")?;
        move_bookmark(
            &ctx,
            &repo,
            &bcs_ids,
            &importing_bookmark,
            &checker_flags,
            &call_sign,
            &None,
            &mut recovery_fields,
        )
        .await?;
        // Check the bookmark moves created BookmarkLogUpdate entries
        let entries = repo
            .bookmark_update_log()
            .list_bookmark_log_entries(
                ctx.clone(),
                BookmarkKey::new("repo_import_test_repo")?,
                5,
                None,
                Freshness::MostRecent,
            )
            .map_ok(|(_id, cs, rs, _ts)| (cs, rs))
            .try_collect::<Vec<_>>()
            .await?;

        assert_eq!(
            entries,
            vec![
                (Some(changesets["G"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["F"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["D"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["B"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["A"]), BookmarkUpdateReason::ManualMove),
            ]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_move_bookmark_with_existing_bookmark(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
        let mut recovery_fields = create_mock_recovery_fields();
        let checker_flags = CheckerFlags {
            phab_check_disabled: true,
            x_repo_check_disabled: true,
        };
        let changesets = create_from_dag(
            &ctx,
            &repo,
            r##"
                A-B-C-D-E-F-G
            "##,
        )
        .await?;

        let bcs_ids: Vec<ChangesetId> = changesets.values().copied().collect();
        let importing_bookmark = BookmarkKey::new("repo_import_test_repo")?;
        let mut txn = repo.bookmarks().create_transaction(ctx.clone());
        txn.create(
            &importing_bookmark,
            bcs_ids.first().unwrap().clone(),
            BookmarkUpdateReason::ManualMove,
        )?;
        txn.commit().await.unwrap();
        move_bookmark(
            &ctx,
            &repo,
            &bcs_ids,
            &importing_bookmark,
            &checker_flags,
            &None,
            &None,
            &mut recovery_fields,
        )
        .await?;
        // Check the bookmark moves created BookmarkLogUpdate entries
        let entries = repo
            .bookmark_update_log()
            .list_bookmark_log_entries(
                ctx.clone(),
                BookmarkKey::new("repo_import_test_repo")?,
                5,
                None,
                Freshness::MostRecent,
            )
            .map_ok(|(_id, cs, rs, _ts)| (cs, rs))
            .try_collect::<Vec<_>>()
            .await?;

        assert_eq!(
            entries,
            vec![
                (Some(changesets["G"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["F"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["D"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["B"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["A"]), BookmarkUpdateReason::ManualMove),
            ]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_merge_push_commit(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo = create_repo(fb, 1).await?;

        let master_cs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let imported_cs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("b", "b")
            .commit()
            .await?;

        let dest_bookmark = bookmark(&ctx, &repo, "master").set_to(master_cs_id).await?;

        let changeset_args = ChangesetArgs {
            author: "user".to_string(),
            message: "merging".to_string(),
            datetime: DateTime::now(),
        };

        let merged_cs_id =
            merge_imported_commit(&ctx, &repo, imported_cs_id, &dest_bookmark, changeset_args)
                .await?;

        let repo_config = RepoConfig {
            pushrebase: PushrebaseParams {
                globalrev_config: Some(GlobalrevConfig {
                    publishing_bookmark: BookmarkKey::new("master")?,
                    globalrevs_small_repo_id: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let pushed_cs_id =
            push_merge_commit(&ctx, &repo, merged_cs_id, &dest_bookmark, &repo_config).await?;
        let pushed_cs = pushed_cs_id.load(&ctx, repo.repo_blobstore()).await?;

        assert_eq!(
            Globalrev::new(START_COMMIT_GLOBALREV),
            Globalrev::from_bcs(&pushed_cs)?
        );
        Ok(())
    }

    const COMMIT_SYNC_ALL: &str = r#"{
        "repos": {
            "large_repo_1": {
                "versions": [{
                    "large_repo_id": 0,
                    "common_pushrebase_bookmarks": ["b1"],
                    "small_repos": [
                        {
                            "repoid": 1,
                            "default_action": "prepend_prefix",
                            "default_prefix": "f1",
                            "bookmark_prefix": "bp1/",
                            "mapping": {"d": "dd"},
                            "direction": "large_to_small"
                        },
                        {
                            "repoid": 2,
                            "default_action": "prepend_prefix",
                            "default_prefix": "f2",
                            "bookmark_prefix": "bp2/",
                            "mapping": {"d": "ddd"},
                            "direction": "small_to_large"
                        }
                    ],
                    "version_name": "TEST_VERSION_NAME_LIVE_1"
                }],
                "current_version": "TEST_VERSION_NAME_LIVE_1",
                "common": {
                    "common_pushrebase_bookmarks": ["b1"],
                    "large_repo_id": 0,
                    "small_repos": {
                      "1": {
                        "bookmark_prefix": "bp1/"
                      },
                      "2": {
                        "bookmark_prefix": "bp2/"
                      }
                    }
                }
            }
        }
    }"#;

    fn insert_repo_config(id: i32, repos: &mut HashMap<String, RepoConfig>) {
        let repo_config = RepoConfig {
            repoid: RepositoryId::new(id),
            ..Default::default()
        };
        repos.insert(format!("repo{id}"), repo_config);
    }

    fn repo_config_by_id(repo_configs: &RepoConfigs, id: RepositoryId) -> Result<RepoConfig> {
        repo_configs
            .iter()
            .map(|(_, config)| config)
            .find(|config| config.repoid == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no config for {id}"))
    }

    /*
        repo0: no push-redirection => None
        repo1: push-redirects to repo0 => Some(repo_config)
        repo2: draft push-redirects => None
    */
    #[mononoke::fbinit_test]
    async fn test_get_large_repo_config_if_pushredirected(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let test_source = Arc::new(TestSource::new());

        let mut repos: HashMap<String, RepoConfig> = HashMap::new();

        test_source.insert_config(
            CONFIGERATOR_ALL_COMMIT_SYNC_CONFIGS,
            COMMIT_SYNC_ALL,
            ModificationTime::UnixTimestamp(0),
        );

        test_source.insert_to_refresh(CONFIGERATOR_ALL_COMMIT_SYNC_CONFIGS.to_string());

        let test_push_redirection_config = Arc::new(TestPushRedirectionConfig::new());
        test_push_redirection_config
            .set(&ctx, RepositoryId::new(1), false, true)
            .await?;
        test_push_redirection_config
            .set(&ctx, RepositoryId::new(2), true, false)
            .await?;

        let config_store = ConfigStore::new(test_source.clone(), Duration::from_millis(2), None);
        let live_commit_sync_config = Arc::new(CfgrLiveCommitSyncConfig::new(
            &config_store,
            test_push_redirection_config,
        )?);

        let repo0 =
            create_repo_with_live_commit_sync_config(fb, 0, live_commit_sync_config.clone())
                .await?;

        insert_repo_config(0, &mut repos);
        let repo_configs = RepoConfigs::new(repos.clone(), CommonConfig::default());
        assert!(
            get_large_repo_config_if_pushredirected(&ctx, &repo0, &live_commit_sync_config, |id| {
                repo_config_by_id(&repo_configs, id)
            },)
            .await?
            .is_none()
        );

        let repo1 =
            create_repo_with_live_commit_sync_config(fb, 1, live_commit_sync_config.clone())
                .await?;

        insert_repo_config(1, &mut repos);
        let repo_configs = RepoConfigs::new(repos.clone(), CommonConfig::default());
        assert!(
            get_large_repo_config_if_pushredirected(&ctx, &repo1, &live_commit_sync_config, |id| {
                repo_config_by_id(&repo_configs, id)
            },)
            .await?
            .is_some()
        );

        let repo2 =
            create_repo_with_live_commit_sync_config(fb, 2, live_commit_sync_config.clone())
                .await?;

        insert_repo_config(2, &mut repos);
        let repo_configs = RepoConfigs::new(repos.clone(), CommonConfig::default());
        assert!(
            get_large_repo_config_if_pushredirected(&ctx, &repo2, &live_commit_sync_config, |id| {
                repo_config_by_id(&repo_configs, id)
            },)
            .await?
            .is_none()
        );
        Ok(())
    }

    fn get_small_repo_sync_config_1() -> SmallRepoCommitSyncConfig {
        SmallRepoCommitSyncConfig {
            default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(mp(
                "large_repo",
            )),
            map: hashmap! {
                mp("dest_path_prefix/B") => mp("random_dir/B"),
            },
            submodule_config: Default::default(),
        }
    }

    fn get_small_repo_sync_config_1_later() -> SmallRepoCommitSyncConfig {
        SmallRepoCommitSyncConfig {
            default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(mp(
                "large_repo",
            )),
            map: hashmap! {
                mp("dest_path_prefix/B") => mp("random_dir/B"),
                mp("dest_path_prefix/C") => mp("random_dir/C"),
            },
            submodule_config: Default::default(),
        }
    }

    fn get_small_repo_sync_config_2() -> SmallRepoCommitSyncConfig {
        SmallRepoCommitSyncConfig {
            default_action: DefaultSmallToLargeCommitSyncPathAction::Preserve,
            map: hashmap! {
                mp("dest_path_prefix_2") => mp("dpp2"),
            },
            submodule_config: Default::default(),
        }
    }

    fn get_large_repo_live_commit_sync_config() -> Arc<dyn LiveCommitSyncConfig> {
        let commit_sync_config = get_large_repo_sync_config();
        let (sync_config, source) = TestLiveCommitSyncConfig::new_with_source();

        let later_commit_sync_config = get_later_large_repo_sync_config();

        source.add_config(commit_sync_config.clone());
        source.add_config(later_commit_sync_config);
        source.add_common_config(CommonCommitSyncConfig {
            common_pushrebase_bookmarks: commit_sync_config.common_pushrebase_bookmarks.clone(),
            small_repos: hashmap! {
                RepositoryId::new(1) => SmallRepoPermanentConfig {
                    bookmark_prefix: AsciiString::from_str("large_repo_bookmark/")
                        .unwrap(),
                    common_pushrebase_bookmarks_map: HashMap::new(),
                },
                RepositoryId::new(2) => SmallRepoPermanentConfig {
                    bookmark_prefix: AsciiString::from_str("large_repo_bookmark_2/")
                        .unwrap(),
                    common_pushrebase_bookmarks_map: HashMap::new(),
                },
            },
            large_repo_id: commit_sync_config.large_repo_id,
        });

        Arc::new(sync_config)
    }

    fn first_version() -> CommitSyncConfigVersion {
        CommitSyncConfigVersion("TEST_VERSION".to_string())
    }

    fn get_large_repo_sync_config() -> CommitSyncConfig {
        CommitSyncConfig {
            large_repo_id: RepositoryId::new(0),
            common_pushrebase_bookmarks: vec![],
            small_repos: hashmap! {
                RepositoryId::new(1) => get_small_repo_sync_config_1(),
                RepositoryId::new(2) => get_small_repo_sync_config_2(),
            },
            version_name: first_version(),
        }
    }

    fn get_later_large_repo_sync_config() -> CommitSyncConfig {
        CommitSyncConfig {
            large_repo_id: RepositoryId::new(0),
            common_pushrebase_bookmarks: vec![],
            small_repos: hashmap! {
                RepositoryId::new(1) => get_small_repo_sync_config_1_later(),
                RepositoryId::new(2) => get_small_repo_sync_config_2(),
            },
            version_name: CommitSyncConfigVersion("TEST_VERSION2".to_string()),
        }
    }

    /*
        The test checks, if get_large_repo_setting function returns the correct
        variables for a large repo setting given small repo settings.
        -> bookmarks are prepended with the bookmark_prefixes given in the configs
    */
    #[mononoke::fbinit_test]
    async fn test_get_large_repo_setting(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let live_commit_sync_config = get_large_repo_live_commit_sync_config();
        let large_repo =
            create_repo_with_live_commit_sync_config(fb, 0, live_commit_sync_config.clone())
                .await?;
        let small_repo_1 =
            create_repo_with_live_commit_sync_config(fb, 1, live_commit_sync_config.clone())
                .await?;

        let small_repo_setting_1 = RepoImportSetting {
            importing_bookmark: create_bookmark_name("importing_bookmark"),
            dest_bookmark: create_bookmark_name("dest_bookmark"),
        };

        let syncers_1 = create_commit_syncers(
            &ctx,
            small_repo_1.clone(),
            large_repo.clone(),
            SubmoduleDeps::ForSync(HashMap::new()),
            live_commit_sync_config.clone(),
        )?;

        let large_repo_setting_1 =
            get_large_repo_setting(&ctx, &small_repo_setting_1, &syncers_1.small_to_large).await?;

        let expected_large_repo_setting_1 = RepoImportSetting {
            importing_bookmark: create_bookmark_name("large_repo_bookmark/importing_bookmark"),
            dest_bookmark: create_bookmark_name("large_repo_bookmark/dest_bookmark"),
        };

        assert_eq!(expected_large_repo_setting_1, large_repo_setting_1);

        let small_repo_2 =
            create_repo_with_live_commit_sync_config(fb, 2, live_commit_sync_config.clone())
                .await?;

        let small_repo_setting_2 = RepoImportSetting {
            importing_bookmark: create_bookmark_name("importing_bookmark_2"),
            dest_bookmark: create_bookmark_name("dest_bookmark_2"),
        };

        let syncers_2 = create_commit_syncers(
            &ctx,
            small_repo_2.clone(),
            large_repo.clone(),
            SubmoduleDeps::ForSync(HashMap::new()),
            live_commit_sync_config,
        )?;

        let large_repo_setting_2 =
            get_large_repo_setting(&ctx, &small_repo_setting_2, &syncers_2.small_to_large).await?;

        let expected_large_repo_setting_2 = RepoImportSetting {
            importing_bookmark: create_bookmark_name("large_repo_bookmark_2/importing_bookmark_2"),
            dest_bookmark: create_bookmark_name("large_repo_bookmark_2/dest_bookmark_2"),
        };

        assert_eq!(expected_large_repo_setting_2, large_repo_setting_2);
        Ok(())
    }

    /*
        The test checks if we have imported the files into the right paths
        in a pushredirected large repo and if we have back-synced the files
        in the right places in small repo.

        Given file A, B and destination prefix "dest_path_prefix".
        If we imported into small repo and allowed it to push-redirect to
        large repo, we would get the following path rewriting sequences:
        A -> dest_path_prefix/A (in small_repo) -> large_repo/dest_path_prefix/A
        B -> dest_path_prefix/B (in small_repo) -> random_dir/B

        Therefore, repo_import tool should import A into large_repo_dest_prefix/A
        and B into random_dir/B places. When we backsync to small_repo, we should get
        dest_path_prefix/A and dest_path_prefix/B paths, respectively.
    */
    #[mononoke::fbinit_test]
    async fn test_rewrite_file_paths_and_backsync(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let live_commit_sync_config = get_large_repo_live_commit_sync_config();
        let large_repo =
            create_repo_with_live_commit_sync_config(fb, 0, live_commit_sync_config.clone())
                .await?;
        let small_repo =
            create_repo_with_live_commit_sync_config(fb, 1, live_commit_sync_config.clone())
                .await?;
        let changesets = create_from_dag(
            &ctx,
            &large_repo,
            r##"
                A-B
            "##,
        )
        .await?;
        let cs_ids: Vec<ChangesetId> = changesets.values().copied().collect();

        let syncers = create_commit_syncers(
            &ctx,
            small_repo.clone(),
            large_repo.clone(),
            SubmoduleDeps::ForSync(HashMap::new()),
            live_commit_sync_config,
        )?;

        let large_to_small_syncer = syncers.large_to_small;
        let mut movers = vec![];
        let importing_mover: Arc<dyn Mover> = Arc::new(CrossRepoMover::new(
            HashMap::new(),
            DefaultAction::PrependPrefix(mp("dest_path_prefix")),
        )?);
        movers.push(importing_mover);
        movers.push(
            syncers
                .small_to_large
                .get_movers_by_version(&CommitSyncConfigVersion("TEST_VERSION".to_string()))
                .await?
                .mover,
        );

        let combined_mover = Arc::new(CombinedMover::new(movers));

        let combined_movers = Movers {
            mover: combined_mover.clone(),
            reverse_mover: Arc::new(InvalidReverseMover),
        };

        let (shifted_bcs_ids, _git_merge_shifted_bcs_id) = rewrite_file_paths(
            &ctx,
            &large_repo,
            &combined_movers,
            &cs_ids,
            cs_ids.last().unwrap(),
        )
        .await?;

        let large_repo_cs_a = &shifted_bcs_ids[0]
            .load(&ctx, large_repo.repo_blobstore())
            .await?;
        let large_repo_cs_a_mpaths = get_file_changes_mpaths(large_repo_cs_a);
        assert_eq!(
            vec![mp("large_repo/dest_path_prefix/A")],
            large_repo_cs_a_mpaths
        );

        let large_repo_cs_b = &shifted_bcs_ids[1]
            .load(&ctx, large_repo.repo_blobstore())
            .await?;
        let large_repo_cs_b_mpaths = get_file_changes_mpaths(large_repo_cs_b);
        assert_eq!(vec![mp("random_dir/B")], large_repo_cs_b_mpaths);

        let synced_bcs_ids = back_sync_commits_to_small_repo(
            &ctx,
            &small_repo,
            &large_to_small_syncer,
            &shifted_bcs_ids,
            &first_version(),
        )
        .await?;

        let small_repo_cs_a = &synced_bcs_ids[0]
            .load(&ctx, small_repo.repo_blobstore())
            .await?;
        let small_repo_cs_a_mpaths = get_file_changes_mpaths(small_repo_cs_a);
        assert_eq!(vec![mp("dest_path_prefix/A")], small_repo_cs_a_mpaths);

        let small_repo_cs_b = &synced_bcs_ids[1]
            .load(&ctx, small_repo.repo_blobstore())
            .await?;
        let small_repo_cs_b_mpaths = get_file_changes_mpaths(small_repo_cs_b);
        assert_eq!(vec![mp("dest_path_prefix/B")], small_repo_cs_b_mpaths);

        Ok(())
    }

    async fn check_no_pending_commits(
        ctx: &CoreContext,
        repo: &Repo,
        cs_ids: &[ChangesetId],
    ) -> Result<()> {
        let derived_data_types = &repo.repo_derived_data().active_config().types;

        for derived_data_type in derived_data_types {
            let pending = repo
                .repo_derived_data()
                .manager()
                .pending(ctx, cs_ids, None, *derived_data_type)
                .await?;
            assert!(pending.is_empty());
        }

        Ok(())
    }

    /*
        Given two repos and their changesets, we check if we have derived all the
        data types for the changesets
    */
    #[mononoke::fbinit_test]
    async fn test_derive_bonsais_multiple_repos(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo_0 = create_repo(fb, 0).await?;

        let repo_0_commits = create_from_dag(
            &ctx,
            &repo_0,
            r##"
                A-B
            "##,
        )
        .await?;

        let repo_0_cs_ids: Vec<ChangesetId> = repo_0_commits.values().copied().collect();

        let repo_1 = create_repo(fb, 1).await?;
        let repo_1_commits = create_from_dag(
            &ctx,
            &repo_1,
            r##"
                C-D
            "##,
        )
        .await?;

        let repo_1_cs_ids: Vec<ChangesetId> = repo_1_commits.values().copied().collect();

        derive_bonsais_single_repo(&ctx, &repo_0, &repo_0_cs_ids).await?;
        derive_bonsais_single_repo(&ctx, &repo_1, &repo_1_cs_ids).await?;

        check_no_pending_commits(&ctx, &repo_0, &repo_0_cs_ids).await?;
        check_no_pending_commits(&ctx, &repo_1, &repo_1_cs_ids).await?;
        Ok(())
    }

    /*
        The test combines rewrite and derive functionalities:
        Given a large repo that backsyncs to small_repo, we check if we have
        derived all the data types for both repos.
    */
    #[mononoke::fbinit_test]
    async fn test_rewrite_and_derive(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let live_commit_sync_config = get_large_repo_live_commit_sync_config();
        let large_repo =
            create_repo_with_live_commit_sync_config(fb, 0, live_commit_sync_config.clone())
                .await?;
        let small_repo =
            create_repo_with_live_commit_sync_config(fb, 1, live_commit_sync_config.clone())
                .await?;
        let changesets = create_from_dag(
            &ctx,
            &large_repo,
            r##"
                A-B
            "##,
        )
        .await?;

        let cs_ids: Vec<ChangesetId> = changesets.values().copied().collect();

        let syncers = create_commit_syncers(
            &ctx,
            small_repo.clone(),
            large_repo.clone(),
            SubmoduleDeps::ForSync(HashMap::new()),
            live_commit_sync_config,
        )?;

        let large_to_small_syncer = syncers.large_to_small;
        let mut movers = vec![];
        let importing_mover: Arc<dyn Mover> = Arc::new(CrossRepoMover::new(
            HashMap::new(),
            DefaultAction::PrependPrefix(mp("dest_path_prefix")),
        )?);
        movers.push(importing_mover);
        movers.push(
            syncers
                .small_to_large
                .get_movers_by_version(&CommitSyncConfigVersion("TEST_VERSION".to_string()))
                .await?
                .mover,
        );

        let combined_mover = Arc::new(CombinedMover::new(movers));
        let combined_movers = Movers {
            mover: combined_mover.clone(),
            reverse_mover: Arc::new(InvalidReverseMover),
        };

        let (large_repo_cs_ids, _) = rewrite_file_paths(
            &ctx,
            &large_repo,
            &combined_movers,
            &cs_ids,
            cs_ids.last().unwrap(),
        )
        .await?;
        let small_repo_cs_ids = back_sync_commits_to_small_repo(
            &ctx,
            &small_repo,
            &large_to_small_syncer,
            &large_repo_cs_ids,
            &first_version(),
        )
        .await?;

        derive_bonsais_single_repo(&ctx, &large_repo, &large_repo_cs_ids).await?;
        derive_bonsais_single_repo(&ctx, &small_repo, &small_repo_cs_ids).await?;

        check_no_pending_commits(&ctx, &large_repo, &large_repo_cs_ids).await?;
        check_no_pending_commits(&ctx, &small_repo, &small_repo_cs_ids).await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_version_and_backsync(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let live_commit_sync_config = get_large_repo_live_commit_sync_config();
        let large_repo =
            create_repo_with_live_commit_sync_config(fb, 0, live_commit_sync_config.clone())
                .await?;
        let small_repo =
            create_repo_with_live_commit_sync_config(fb, 1, live_commit_sync_config.clone())
                .await?;

        let root = CreateCommitContext::new_root(&ctx, &large_repo)
            .add_file("random_dir/B/file", "text")
            .commit()
            .await?;

        let first_commit = CreateCommitContext::new(&ctx, &large_repo, vec![root])
            .add_file("large_repo/justfile", "justtext")
            .commit()
            .await?;

        bookmark(&ctx, &large_repo, "before_mapping_change")
            .set_to(first_commit)
            .await?;

        let syncers = create_commit_syncers(
            &ctx,
            small_repo.clone(),
            large_repo.clone(),
            SubmoduleDeps::ForSync(HashMap::new()),
            live_commit_sync_config,
        )?;

        let large_to_small_syncer = syncers.large_to_small;

        let small_repo_cs_ids = back_sync_commits_to_small_repo(
            &ctx,
            &small_repo,
            &large_to_small_syncer,
            &[root, first_commit],
            &first_version(),
        )
        .await?;

        let wc = list_working_copy_utf8(&ctx, &small_repo, small_repo_cs_ids[0]).await?;
        assert_eq!(
            wc,
            hashmap! {
                NonRootMPath::new("dest_path_prefix/B/file")? => "text".to_string()
            }
        );

        let wc = list_working_copy_utf8(&ctx, &small_repo, small_repo_cs_ids[1]).await?;
        assert_eq!(
            wc,
            hashmap! {
                NonRootMPath::new("dest_path_prefix/B/file")? => "text".to_string(),
                NonRootMPath::new("justfile")? => "justtext".to_string(),
            }
        );

        // Change mapping
        let change_mapping_cs_id = CreateCommitContext::new(&ctx, &large_repo, vec![first_commit])
            .commit()
            .await?;
        bookmark(&ctx, &large_repo, "after_mapping_change")
            .set_to(change_mapping_cs_id)
            .await?;

        unsafe_always_rewrite_sync_commit(
            &ctx,
            change_mapping_cs_id,
            &large_to_small_syncer,
            None,
            &CommitSyncConfigVersion("TEST_VERSION2".to_string()),
            CommitSyncContext::Tests,
        )
        .await?;

        assert_eq!(
            find_mapping_version(
                &ctx,
                &large_to_small_syncer,
                &BookmarkKey::new("before_mapping_change")?,
            )
            .await?,
            Some(CommitSyncConfigVersion("TEST_VERSION".to_string()))
        );

        assert_eq!(
            find_mapping_version(
                &ctx,
                &large_to_small_syncer,
                &BookmarkKey::new("after_mapping_change")?,
            )
            .await?,
            Some(CommitSyncConfigVersion("TEST_VERSION2".to_string()))
        );

        Ok(())
    }

    fn hg_manifest_knobs(read_from_augmented: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(read_from_augmented),
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        })
    }

    /// A repo whose Hg manifest reads fail once the blocker is armed, and a
    /// commit with an hg changeset but no augmented manifest.
    async fn repo_with_blocked_hg_manifests(
        ctx: &CoreContext,
    ) -> Result<(Repo, Arc<HgManifestReadBlocker>, ChangesetId)> {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let repo: Repo = TestRepoFactory::new(ctx.fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        let cs_id = CreateCommitContext::new_root(ctx, &repo)
            .add_file("a", "a")
            .add_file("dir/b", "b")
            .commit()
            .await?;
        with_just_knobs_async(
            hg_manifest_knobs(false),
            repo.derive_hg_changeset(ctx, cs_id).boxed(),
        )
        .await?;
        Ok((repo, blocker, cs_id))
    }

    #[mononoke::fbinit_test]
    async fn test_leaf_entries_from_augmented(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker, cs_id) = repo_with_blocked_hg_manifests(&ctx).await?;
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, cs_id, DerivationPriority::LOW)
            .await?;

        // Given: the files listed from the Hg manifest.
        let from_hg = with_just_knobs_async(
            hg_manifest_knobs(false),
            get_leaf_entries(&ctx, &repo, cs_id).boxed(),
        )
        .await?;
        assert_eq!(from_hg, [mp("a"), mp("dir/b")].into_iter().collect());

        // When: Hg manifests can no longer be read.
        blocker.arm();

        // Then: the augmented manifest lists the same files.
        let from_augmented = with_just_knobs_async(
            hg_manifest_knobs(true),
            get_leaf_entries(&ctx, &repo, cs_id).boxed(),
        )
        .await?;
        assert_eq!(from_augmented, from_hg);
        assert_eq!(
            blocker.reads(),
            0,
            "no Hg manifest is read with the knob on"
        );

        // And: with the knob off, the Hg manifest is still read.
        let knob_off = with_just_knobs_async(
            hg_manifest_knobs(false),
            get_leaf_entries(&ctx, &repo, cs_id).boxed(),
        )
        .await;
        assert!(
            knob_off.is_err(),
            "the knob off reads the blocked Hg manifest"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_leaf_entries_missing_augmented(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker, cs_id) = repo_with_blocked_hg_manifests(&ctx).await?;
        blocker.arm();

        let err = with_just_knobs_async(
            hg_manifest_knobs(true),
            get_leaf_entries(&ctx, &repo, cs_id).boxed(),
        )
        .await
        .expect_err("a missing augmented manifest is an error");
        assert!(
            format!("{err:#}").contains("hgaugmentedmanifest.sha1."),
            "the error names the missing augmented manifest: {err:#}"
        );
        assert_eq!(
            blocker.reads(),
            0,
            "there is no fallback to the Hg manifest"
        );
        Ok(())
    }
}
