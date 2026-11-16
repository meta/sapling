/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Error;
use blobstore::Loadable;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::btreemap;
use maplit::hashmap;
use megarepo_config::MononokeMegarepoConfigs;
use megarepo_config::Target;
use megarepo_mapping::REMAPPING_STATE_FILE;
use megarepo_mapping::SourceName;
use metaconfig_types::RepoConfigArc;
use mononoke_api::MononokeRepo;
use mononoke_macros::mononoke;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;
use repo_blobstore::RepoBlobstoreRef;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tests_utils::list_working_copy_utf8_with_types;
use tests_utils::resolve_cs_id;

use crate::add_sync_target::AddSyncTarget;
use crate::change_target_config::ChangeTargetConfig;
use crate::common::MegarepoOp;
use crate::common::SYNC_TARGET_CONFIG_FILE;
use crate::megarepo_test_utils::MegarepoTest;
use crate::megarepo_test_utils::SyncTargetConfigBuilder;

#[mononoke::fbinit_test]
async fn test_change_target_config(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;

    let version_2 = "version_2".to_string();
    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("third", "third")
        .commit()
        .await?;

    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first_in_other_location")
        .build_source()?
        .source_builder(third_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .build_source()?
        .build(&mut test.configs_storage);

    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let mut wc = list_working_copy_utf8_with_types(&ctx, &test.repo, target_cs_id).await?;

    // Remove file with commit remapping state because it's never present in source
    assert!(
        wc.remove(&NonRootMPath::new(REMAPPING_STATE_FILE)?)
            .is_some()
    );
    assert!(
        wc.remove(&NonRootMPath::new(SYNC_TARGET_CONFIG_FILE)?)
            .is_some()
    );

    assert_eq!(
        wc,
        hashmap! {
            NonRootMPath::new("linkfiles/first_in_other_location")? => ("../source_1/first".to_string(), FileType::Symlink),
            NonRootMPath::new("source_1/first")? => ("first".to_string(), FileType::Regular),
            NonRootMPath::new("source_3/third")? => ("third".to_string(), FileType::Regular),
        }
    );

    let target_bonsai = target_cs_id.load(&ctx, &test.repo.repo_blobstore()).await?;
    assert_eq!(
        target_bonsai
            .file_changes()
            .map(|(a, _b)| a.clone())
            .collect::<Vec<_>>(),
        vec![
            NonRootMPath::new(".megarepo/remapping_state")?,
            NonRootMPath::new(".megarepo/sync_target_config")?,
            NonRootMPath::new("linkfiles/first")?,
            NonRootMPath::new("linkfiles/second")?,
            NonRootMPath::new("source_2/second")?
        ],
    );

    Ok(())
}

async fn init_megarepo<R: MononokeRepo>(
    ctx: &CoreContext,
    test: &mut MegarepoTest<R>,
) -> Result<(), Error> {
    let first_source_name = SourceName::new("source_1");
    let second_source_name = SourceName::new("source_2");
    let version = "version_1".to_string();
    let target: Target = test.target("target".to_string());

    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first")
        .build_source()?
        .source_builder(second_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("second", "linkfiles/second")
        .build_source()?
        .build(&mut test.configs_storage);

    println!("Create initial source commits and bookmarks");
    let first_source_cs_id = CreateCommitContext::new_root(ctx, &test.repo)
        .add_file("first", "first")
        .commit()
        .await?;

    bookmark(ctx, &test.repo, first_source_name.to_string())
        .set_to(first_source_cs_id)
        .await?;

    let second_source_cs_id = CreateCommitContext::new_root(ctx, &test.repo)
        .add_file("second", "second")
        .commit()
        .await?;

    bookmark(ctx, &test.repo, second_source_name.to_string())
        .set_to(second_source_cs_id)
        .await?;

    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());

    let add_sync_target = AddSyncTarget::new(&configs_storage, &test.mononoke);
    let repo = add_sync_target.find_repo_by_id(ctx, target.repo_id).await?;
    let repo_config = repo.repo().repo_config_arc();

    let sync_target_config = test
        .configs_storage
        .get_config_by_version(ctx.clone(), repo_config, target.clone(), version.clone())
        .await?;
    add_sync_target
        .run(
            ctx,
            sync_target_config,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                second_source_name.clone() => second_source_cs_id,
            },
            None,
        )
        .await?;

    Ok(())
}

fn knobs(read_from_augmented: bool, create_augmented: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(read_from_augmented),
        "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(create_augmented),
    })
}

/// The working copy of the target after the config change of
/// `test_change_target_config`, made with `knobs` set, and the file changes of
/// the target's last commit: the merge rebuilt on new parents.
async fn target_after_config_change(
    fb: FacebookInit,
    knobs: JustKnobsInMemory,
) -> Result<
    (
        HashMap<NonRootMPath, (String, FileType)>,
        Vec<(NonRootMPath, FileChange)>,
    ),
    Error,
> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;

    let version_2 = "version_2".to_string();
    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("third", "third")
        .commit()
        .await?;

    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first_in_other_location")
        .build_source()?
        .source_builder(third_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .build_source()?
        .build(&mut test.configs_storage);

    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    with_just_knobs_async(
        knobs,
        change_target_config
            .run(
                &ctx,
                &target,
                version_2,
                target_cs_id,
                btreemap! {
                    first_source_name.clone() => first_source_cs_id,
                    third_source_name.clone() => third_source_cs_id,
                },
                None,
            )
            .boxed(),
    )
    .await?;

    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let wc = list_working_copy_utf8_with_types(&ctx, &test.repo, target_cs_id).await?;
    let file_changes = target_cs_id
        .load(&ctx, &test.repo.repo_blobstore())
        .await?
        .file_changes()
        .map(|(path, change)| (path.clone(), change.clone()))
        .collect();
    Ok((wc, file_changes))
}

#[mononoke::fbinit_test]
async fn test_change_target_config_reads_augmented_manifests(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Given: the target after the config change, with the merge rebuild
    // diffing Hg manifests.
    let from_hg = target_after_config_change(fb, knobs(false, true)).await?;

    // When: the rebuild diffs augmented manifests instead, and deriving the hg
    // changeset of the merge it diffs also creates the merge's augmented
    // manifest.
    let from_augmented = target_after_config_change(fb, knobs(true, true)).await?;

    // Then: the target is the same.
    assert_eq!(from_augmented, from_hg);

    // And: when deriving the hg changeset does not create it, the merge's
    // augmented manifest is still derived first, because Hg changesets depend
    // on augmented manifests, so the target is the same.
    let without_creation = target_after_config_change(fb, knobs(true, false)).await?;
    assert_eq!(without_creation, from_hg);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_target_config_invalid_config_linkfile(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("third", "third")
        .commit()
        .await?;
    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;

    let version_2 = "version_2".to_string();
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first")
        .build_source()?
        .source_builder(third_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        // NOTE - intentionally overwrite existing linkfiles to make sure the request fails
        .linkfile("third", "linkfiles/first")
        .build_source()?
        .build(&mut test.configs_storage);

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    let err = change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await;

    assert!(
        format!("{}", err.unwrap_err())
            .contains("path linkfiles/first cannot be added to the target - it's already present")
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_target_config_invalid_config_normal_file(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("first", "someothercontent")
        .commit()
        .await?;
    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;

    let version_2 = "version_2".to_string();
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first")
        .build_source()?
        .source_builder(third_source_name.clone())
        // NOTE - intentionally overwrite existing paths to make sure the request fails
        .default_prefix("source_1")
        .bookmark("source_3")
        .build_source()?
        .build(&mut test.configs_storage);

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    let err = change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await;

    assert!(
        format!("{}", err.unwrap_err())
            .contains("path source_1/first cannot be added to the target - it's already present")
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_target_config_invalid_config_file_dir_conflict(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("first", "someothercontent")
        .commit()
        .await?;
    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;

    let version_2 = "version_2".to_string();
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first")
        .build_source()?
        .source_builder(third_source_name.clone())
        // NOTE - invalid config - conflict between "source_1/first/first" from source_3 and
        // "source_1/first" from source_1
        .default_prefix("source_1/first")
        .bookmark("source_3")
        .build_source()?
        .build(&mut test.configs_storage);

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    let err = change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await;

    assert!(
        format!("{}", err.unwrap_err())
            .contains("File in target path source_1/first conflicts with newly added files",)
    );

    Ok(())
}

// Replace "source_1/first" with "source_1/first/first"
#[mononoke::fbinit_test]
async fn test_change_target_config_no_file_dir_conflict(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("first", "someothercontent")
        .commit()
        .await?;
    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;

    let version_2 = "version_2".to_string();
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(third_source_name.clone())
        .default_prefix("source_1/first")
        .bookmark("source_3")
        .build_source()?
        .build(&mut test.configs_storage);

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    Ok(())
}

// Replace "source_1/dir/first" with "source_1/dir"
#[mononoke::fbinit_test]
async fn test_change_target_config_no_file_dir_conflict_2(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    let version = "version_1".to_string();

    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first")
        .build_source()?
        .build(&mut test.configs_storage);

    println!("Create initial source commits and bookmarks");
    let first_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("dir/first", "first")
        .commit()
        .await?;

    bookmark(&ctx, &test.repo, first_source_name.to_string())
        .set_to(first_source_cs_id)
        .await?;

    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());

    let add_sync_target = AddSyncTarget::new(&configs_storage, &test.mononoke);
    let repo = add_sync_target
        .find_repo_by_id(&ctx, target.repo_id)
        .await?;
    let repo_config = repo.repo().repo_config_arc();

    let sync_target_config = test
        .configs_storage
        .get_config_by_version(ctx.clone(), repo_config, target.clone(), version.clone())
        .await?;
    add_sync_target
        .run(
            &ctx,
            sync_target_config,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
            },
            None,
        )
        .await?;

    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("dir", "anothercontent")
        .commit()
        .await?;
    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;

    let version_2 = "version_2".to_string();
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(third_source_name.clone())
        .default_prefix("source_1")
        .bookmark("source_3")
        .build_source()?
        .build(&mut test.configs_storage);

    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    println!("changing target");
    change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_target_config_repeat_same_request(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;

    let version_2 = "version_2".to_string();
    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("third", "third")
        .commit()
        .await?;

    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first_in_other_location")
        .build_source()?
        .source_builder(third_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .build_source()?
        .build(&mut test.configs_storage);

    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    change_target_config
        .run(
            &ctx,
            &target,
            version_2.clone(),
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    // Now repeat the same request - it should succeed
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    change_target_config
        .run(
            &ctx,
            &target,
            version_2.clone(),
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    // Now send slightly different request - it should fail
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    assert!(
        change_target_config
            .run(
                &ctx,
                &target,
                version_2,
                target_cs_id,
                btreemap! {
                    first_source_name.clone() => first_source_cs_id,
                },
                None,
            )
            .await
            .is_err()
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_target_config_noop_change(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;

    let version_2 = "version_2".to_string();
    let third_source_name = SourceName::new("source_3");
    let third_source_cs_id = CreateCommitContext::new_root(&ctx, &test.repo)
        .add_file("third", "third")
        .commit()
        .await?;

    bookmark(&ctx, &test.repo, third_source_name.to_string())
        .set_to(third_source_cs_id)
        .await?;
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .linkfile("first", "linkfiles/first_in_other_location")
        .build_source()?
        .source_builder(third_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .build_source()?
        .build(&mut test.configs_storage);

    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    let new_target_cs_id = change_target_config
        .run(
            &ctx,
            &target,
            version_2.clone(),
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    // Now do a noop change on existing commit
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    let noop_change = change_target_config
        .run(
            &ctx,
            &target,
            version_2.clone(),
            new_target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
                third_source_name.clone() => third_source_cs_id,
            },
            None,
        )
        .await?;

    assert_ne!(noop_change, new_target_cs_id);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_target_config_linkfile_to_file_mapped_to_multiple_paths(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut test = MegarepoTest::new(&ctx).await?;
    let first_source_name = SourceName::new("source_1");
    let target: Target = test.target("target".to_string());

    init_megarepo(&ctx, &mut test).await?;

    let version_2 = "version_2".to_string();
    SyncTargetConfigBuilder::new(test.repo_id(), target.clone(), version_2.clone())
        .source_builder(first_source_name.clone())
        .set_prefix_bookmark_to_source_name()
        .copyfile("first", "copy_of_first")
        .linkfile("first", "linkfiles/first")
        .build_source()?
        .build(&mut test.configs_storage);

    let first_source_cs_id = resolve_cs_id(&ctx, &test.repo, first_source_name.0.clone()).await?;
    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let configs_storage: Arc<dyn MononokeMegarepoConfigs> = Arc::new(test.configs_storage.clone());
    let change_target_config = ChangeTargetConfig::new(&configs_storage, &test.mononoke);
    change_target_config
        .run(
            &ctx,
            &target,
            version_2,
            target_cs_id,
            btreemap! {
                first_source_name.clone() => first_source_cs_id,
            },
            None,
        )
        .await
        .unwrap();

    let target_cs_id = resolve_cs_id(&ctx, &test.repo, "target").await?;
    let mut wc = list_working_copy_utf8_with_types(&ctx, &test.repo, target_cs_id).await?;

    // Remove file with commit remapping state because it's never present in source
    assert!(
        wc.remove(&NonRootMPath::new(REMAPPING_STATE_FILE)?)
            .is_some()
    );
    assert!(
        wc.remove(&NonRootMPath::new(SYNC_TARGET_CONFIG_FILE)?)
            .is_some()
    );

    assert_eq!(
        wc,
        hashmap! {
            NonRootMPath::new("source_1/first")? => ("first".to_string(), FileType::Regular),
            NonRootMPath::new("linkfiles/first")? => ("../source_1/first".to_string(), FileType::Symlink),
            NonRootMPath::new("copy_of_first")? => ("first".to_string(), FileType::Regular),
        }
    );

    Ok(())
}
