/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

/// Changelists 1 to 5, each a revision of one file, whose server cannot read the revisions
/// of `unreadable_changes`.
fn five_changelists(unreadable_changes: &[u64]) -> FakeSource {
    let source = (1..=5).fold(FakeSource::default(), |source, number| {
        let action = if number == 1 {
            Action::Add
        } else {
            Action::Edit
        };
        source.with_changelist(
            number,
            &[("//test1/a.txt", number as u32, action, Some(b"text\n"))],
        )
    });
    FakeSource {
        unreadable: unreadable_changes
            .iter()
            .map(|number| format!("//test1/a.txt#{number}"))
            .collect(),
        ..source
    }
}

fn committing_before_failure() -> ImportOptions {
    ImportOptions {
        commit_before_failure: true,
        ..Default::default()
    }
}

fn logged_changes(rows: &[Value]) -> Vec<u64> {
    rows.iter()
        .map(|row| row["p4_change"].as_u64().unwrap())
        .collect()
}

#[mononoke::fbinit_test]
async fn test_changelists_built_before_a_failure_are_committed(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let master = BookmarkKey::new("master")?;
    let options = committing_before_failure();
    let uninterrupted = {
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let source = five_changelists(&[]);
        let ctx = CoreContext::test_mock(fb);
        changes_and_ids(&import_next(&ctx, &repo, &source, DEPOT, &master, 10, &options).await?)
    };
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = import_next(
        &ctx,
        &repo,
        &five_changelists(&[3]),
        DEPOT,
        &master,
        10,
        &options,
    )
    .await
    .unwrap_err();

    assert_eq!(Some(Stage::Build), failed_stage(&error), "{error:?}");
    assert!(committed_before_failure(&error), "{error:?}");
    assert_eq!(
        Some(3),
        error.downcast_ref::<BuildingChangelist>().map(|c| c.0),
        "{error:?}"
    );
    assert_eq!(Some(2), resume_point(&ctx, &repo, &master).await?);
    for &(change, cs_id) in &uninterrupted[..2] {
        assert_eq!(Some(cs_id), mapped(&ctx, &repo, change).await?);
    }
    assert_eq!(None, mapped(&ctx, &repo, 3).await?);
    let imported = scuba_rows(&dir, "P4 changelist imported")?;
    assert_eq!(vec![1, 2], logged_changes(&imported));
    let failed = scuba_rows(&dir, "P4 batch failed")?;
    assert_eq!(failed.len(), 1, "{failed:?}");
    let row = &failed[0];
    assert_eq!(row["stage"], "build");
    assert_eq!(row["failed_change"], 3);
    assert_eq!(row["first_change"], 3);
    assert_eq!(row["last_change"], 5);
    assert_eq!(row["batch_changelists"], 3);

    // Once the server can read it, the next import starts at the changelist that failed.
    let batch = import_next(
        &ctx,
        &repo,
        &five_changelists(&[]),
        DEPOT,
        &master,
        10,
        &options,
    )
    .await?;
    assert_eq!(uninterrupted[2..], changes_and_ids(&batch));
    assert_eq!(
        Some(uninterrupted[4].1),
        bookmark_target(&ctx, &repo, &master).await?
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_failure_of_the_first_changelist_commits_nothing(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = five_changelists(&[1]);

    let error = import_next(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        10,
        &committing_before_failure(),
    )
    .await
    .unwrap_err();

    assert_eq!(Some(Stage::Build), failed_stage(&error), "{error:?}");
    assert!(!committed_before_failure(&error), "{error:?}");
    assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    assert!(scuba_rows(&dir, "P4 changelist imported")?.is_empty());
    let failed = scuba_rows(&dir, "P4 batch failed")?;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["failed_change"], 1);
    assert_eq!(failed[0]["first_change"], 1);
    assert_eq!(failed[0]["last_change"], 5);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_by_default_a_failure_commits_nothing(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let options = ImportOptions::default();

    let error = import_next(
        &ctx,
        &repo,
        &five_changelists(&[3]),
        DEPOT,
        &master,
        10,
        &options,
    )
    .await
    .unwrap_err();

    assert_eq!(Some(Stage::Build), failed_stage(&error), "{error:?}");
    assert!(!committed_before_failure(&error), "{error:?}");
    assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    assert_eq!(None, mapped(&ctx, &repo, 1).await?);
    assert!(scuba_rows(&dir, "P4 changelist imported")?.is_empty());
    let failed = scuba_rows(&dir, "P4 batch failed")?;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["failed_change"], 3);
    assert_eq!(failed[0]["first_change"], 1);
    Ok(())
}

/// `max_build_time` ends the batch before the failing changelist, which then fails the next
/// batch on its own.
#[mononoke::fbinit_test]
async fn test_batch_cut_before_a_failure_commits_and_the_next_fails_alone(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    tokio::time::pause();
    let source = FakeSource {
        slow_describes: HashMap::from([(2, Duration::from_secs(601))]),
        ..five_changelists(&[3])
    };
    let options = ImportOptions {
        max_build_time: Duration::from_secs(600),
        ..committing_before_failure()
    };

    let cut = import_next(&ctx, &repo, &source, DEPOT, &master, 10, &options).await?;
    let error = import_next(&ctx, &repo, &source, DEPOT, &master, 10, &options)
        .await
        .unwrap_err();

    let changes: Vec<_> = changes_and_ids(&cut).iter().map(|(c, _)| *c).collect();
    assert_eq!(vec![1, 2], changes);
    assert_eq!(
        Some(3),
        error.downcast_ref::<BuildingChangelist>().map(|c| c.0),
        "{error:?}"
    );
    assert_eq!(Some(2), resume_point(&ctx, &repo, &master).await?);
    Ok(())
}

/// The build failure and the commit's both reach Scuba; the commit's is returned.
#[mononoke::fbinit_test]
async fn test_changelists_before_a_failure_that_fail_to_commit(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let elsewhere = BonsaiP4MappingEntry::new(ONES_CSID, P4ChangelistId::new(2));
    repo.bonsai_p4_mapping
        .bulk_import(&ctx, &[elsewhere])
        .await?;

    let error = import_changelists(
        &ctx,
        &repo,
        &five_changelists(&[3]),
        DEPOT,
        &master,
        &[1, 2, 3, 4, 5],
        &committing_before_failure(),
    )
    .await
    .unwrap_err();

    assert_eq!(Some(Stage::Commit), failed_stage(&error), "{error:?}");
    assert!(!committed_before_failure(&error), "{error:?}");
    assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    let failed = scuba_rows(&dir, "P4 batch failed")?;
    let stages: Vec<_> = failed
        .iter()
        .map(|row| (row["stage"].as_str().unwrap(), row["first_change"].as_u64()))
        .collect();
    assert_eq!(vec![("build", Some(3)), ("commit", Some(1))], stages);
    assert!(scuba_rows(&dir, "P4 changelist imported")?.is_empty());
    Ok(())
}
