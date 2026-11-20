/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `SourceRevisionPushrebaseHook` at the point pushrebase runs it: on the
//! commits as rebased, merge-resolved and saved, before the bookmark moves.

use metaconfig_types::SourceRevisionGuardConfig;
use mononoke_types::SubtreeChange;
use source_revision_pushrebase_hook::EntryChange;
use source_revision_pushrebase_hook::EntryRevisions;
use source_revision_pushrebase_hook::GuardCounters;
use source_revision_pushrebase_hook::GuardCounts;
use source_revision_pushrebase_hook::GuardOutcome;
use source_revision_pushrebase_hook::SnapshotError;
use source_revision_pushrebase_hook::SourceRevision;
use source_revision_pushrebase_hook::SourceRevisionGuardError;
use source_revision_pushrebase_hook::SourceRevisionPushrebaseHook;
use source_revision_pushrebase_hook::TransitionError;

use super::*;

const MODULES: [&str; 3] = ["default", "debug_tools", "onboarding"];
const AUTHORITATIVE: &str = "www/flib/whatsapp/mms/dynamic_strings/generated";
const MIRROR: &str = "whatsapp/common/data/dynamic_strings/generated";

fn init_just_knobs_for_guard_test() {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_replay_from_fork_point".to_string() => KnobVal::Int(0),
        "scm/mononoke:pushrebase_max_merge_conflicts".to_string() => KnobVal::Int(100),
        "scm/mononoke:pushrebase_max_merge_file_size".to_string() => KnobVal::Int(1_000_000),
        "scm/mononoke:pushrebase_far_root_manifest_conflicts".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_reject_noop_merge_commits".to_string() => KnobVal::Bool(false),
    }));
}

fn guard_config() -> Result<SourceRevisionGuardConfig> {
    Ok(SourceRevisionGuardConfig {
        bookmark: master_bookmark(),
        directories: vec![
            NonRootMPath::new(AUTHORITATIVE)?,
            NonRootMPath::new(MIRROR)?,
        ],
        max_files: 64,
        max_file_bytes: 1 << 20,
        max_total_bytes: 1 << 22,
    })
}

fn guard(repo: &PushrebaseTestRepo) -> Result<Vec<Box<dyn PushrebaseHook>>> {
    Ok(vec![SourceRevisionPushrebaseHook::try_new(
        &guard_config()?,
        repo,
    )?])
}

/// The generated files: each module's strings, one per line with enough
/// unchanged lines around them that edits to different strings, or to the
/// revision, merge cleanly.
#[derive(Clone)]
struct Generated {
    revision: Option<&'static str>,
    strings: [[&'static str; 3]; 3],
}

const BASE_STRINGS: [[&str; 3]; 3] = [["d1", "d2", "d3"], ["t1", "t2", "t3"], ["o1", "o2", "o3"]];

impl Generated {
    fn new(revision: Option<&'static str>) -> Self {
        Self {
            revision,
            strings: BASE_STRINGS,
        }
    }

    fn with(mut self, module: usize, index: usize, value: &'static str) -> Self {
        self.strings[module][index] = value;
        self
    }

    fn module_json(&self, module: usize) -> String {
        let revision = self.revision.map_or_else(String::new, |revision| {
            format!("    \"source_revision\": {revision},\n")
        });
        let strings: String = self.strings[module]
            .iter()
            .map(|value| format!("        \"{value}\",\n"))
            .collect();
        format!(
            "{{\n{revision}    \"a\": 1,\n    \"b\": 2,\n    \"c\": 3,\n    \"key\": \"{key}\",\n    \"strings\": [\n{strings}        \"end\"\n    ]\n}}\n",
            key = MODULES[module],
        )
    }

    fn files(&self) -> Vec<(String, String)> {
        (0..MODULES.len())
            .flat_map(|module| {
                let json = self.module_json(module);
                [AUTHORITATIVE, MIRROR]
                    .map(|dir| (format!("{dir}/{}.json", MODULES[module]), json.clone()))
            })
            .collect()
    }
}

async fn commit(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    parents: Vec<ChangesetId>,
    files: Vec<(String, String)>,
) -> Result<ChangesetId> {
    files
        .into_iter()
        .fold(
            CreateCommitContext::new(ctx, repo, parents),
            |commit, (path, content)| commit.add_file(path.as_str(), content),
        )
        .commit()
        .await
}

async fn generate(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    parent: ChangesetId,
    generated: &Generated,
) -> Result<ChangesetId> {
    commit(ctx, repo, vec![parent], generated.files()).await
}

/// A root commit with the generated files, under `master`.
async fn init_master(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    generated: Option<&Generated>,
) -> Result<ChangesetId> {
    let files = generated.map_or_else(Vec::new, Generated::files);
    let root = commit(
        ctx,
        repo,
        vec![],
        [("README".to_string(), "root".to_string())]
            .into_iter()
            .chain(files)
            .collect(),
    )
    .await?;
    bookmark(ctx, repo, master_bookmark()).set_to(root).await?;
    Ok(root)
}

async fn land(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    hooks: &[Box<dyn PushrebaseHook>],
    head: ChangesetId,
) -> Result<PushrebaseOutcome, PushrebaseError> {
    let flags = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..Default::default()
    };
    let pushed = head
        .load(ctx, repo.repo_blobstore())
        .await
        .map_err(Error::from)?;
    do_pushrebase_bonsai(
        ctx,
        repo,
        &flags,
        &master_bookmark(),
        &hashset![pushed],
        hooks,
        None,
    )
    .await
}

async fn master(ctx: &CoreContext, repo: &PushrebaseTestRepo) -> Result<ChangesetId> {
    resolve_cs_id(ctx, repo, "master").await
}

/// Derive content manifests, as derived data would have by the time a
/// batch runs.
async fn derive(ctx: &CoreContext, repo: &PushrebaseTestRepo, ids: &[ChangesetId]) -> Result<()> {
    for id in ids {
        repo.repo_derived_data()
            .derive::<RootContentManifestId>(ctx, *id, DerivationPriority::HIGH)
            .await?;
    }
    Ok(())
}

async fn generated_at(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    cs_id: ChangesetId,
) -> Result<String> {
    let files = list_working_copy_utf8(ctx, repo, cs_id).await?;
    Ok(files[&NonRootMPath::new(format!("{AUTHORITATIVE}/default.json"))?].clone())
}

/// The transition error a rejected pushrebase carries.
fn transition_error(result: Result<PushrebaseOutcome, PushrebaseError>) -> TransitionError {
    match result {
        Err(PushrebaseError::Error(error)) => match error
            .downcast::<SourceRevisionGuardError>()
            .expect("rejected by the source_revision guard")
        {
            SourceRevisionGuardError::InvalidTransition { error, .. } => error,
            other => panic!("expected an invalid transition, got: {other}"),
        },
        Err(other) => panic!("expected a source_revision rejection, got: {other}"),
        Ok(outcome) => panic!("expected a rejection, landed {}", outcome.head),
    }
}

fn guard_error(result: Result<PushrebaseOutcome, PushrebaseError>) -> SourceRevisionGuardError {
    match result {
        Err(PushrebaseError::Error(error)) => error
            .downcast::<SourceRevisionGuardError>()
            .expect("rejected by the source_revision guard"),
        Err(other) => panic!("expected a source_revision rejection, got: {other}"),
        Ok(outcome) => panic!("expected a rejection, landed {}", outcome.head),
    }
}

fn revision(value: i64) -> SourceRevision {
    SourceRevision::new(value).expect("test revisions are positive")
}

#[mononoke::fbinit_test]
async fn guard_ignores_commits_outside_the_generated_files(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let unrelated = commit(&ctx, &repo, vec![root], vec![("x".into(), "x".into())]).await?;
    land(&ctx, &repo, &guard(&repo)?, unrelated).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn guard_accepts_the_next_revision_and_a_no_op_regeneration(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let next = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    land(&ctx, &repo, &hooks, next).await?;

    // Same content, reformatted: the revision must stay.
    let head = master(&ctx, &repo).await?;
    let reformatted: Vec<_> = Generated::new(Some("6"))
        .with(0, 0, "x")
        .files()
        .into_iter()
        .map(|(path, json)| (path, json.replace("    ", "\t")))
        .collect();
    let no_op = commit(&ctx, &repo, vec![head], reformatted.clone()).await?;
    land(&ctx, &repo, &hooks, no_op).await?;

    let head = master(&ctx, &repo).await?;
    let bumped: Vec<_> = reformatted
        .into_iter()
        .map(|(path, json)| {
            (
                path,
                json.replace("\"source_revision\": 6", "\"source_revision\": 7"),
            )
        })
        .collect();
    let bump_only = commit(&ctx, &repo, vec![head], bumped).await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, bump_only).await),
        TransitionError::RevisionChangedWithoutContentChange {
            parent_revision: revision(6),
            revision: revision(7),
        }
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn guard_lets_unversioned_files_change_until_they_start_at_one(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(None))).await?;

    let legacy = generate(&ctx, &repo, root, &Generated::new(None).with(0, 0, "x")).await?;
    land(&ctx, &repo, &hooks, legacy).await?;

    let head = master(&ctx, &repo).await?;
    let two = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("2")).with(0, 0, "x"),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, two).await),
        TransitionError::InvalidInitialRevision {
            revision: revision(2),
        }
    );

    let one = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("1")).with(0, 0, "x"),
    )
    .await?;
    land(&ctx, &repo, &hooks, one).await?;

    let head = master(&ctx, &repo).await?;
    let removed = generate(&ctx, &repo, head, &Generated::new(None).with(0, 0, "y")).await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, removed).await),
        TransitionError::RevisionRemoved {
            parent_revision: revision(1),
        }
    );

    let deleted = Generated::new(None)
        .files()
        .into_iter()
        .fold(
            CreateCommitContext::new(&ctx, &repo, vec![head]),
            |commit, (path, _)| commit.delete_file(path.as_str()),
        )
        .commit()
        .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, deleted).await),
        TransitionError::RevisionRemoved {
            parent_revision: revision(1),
        },
        "deleting the files removes the revision too"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn guard_rejects_invalid_snapshots(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let six = Generated::new(Some("6")).with(0, 0, "x");
    let default = |dir: &str| format!("{dir}/default.json");

    // One module versioned, the others not.
    let mixed = commit(
        &ctx,
        &repo,
        vec![root],
        Generated::new(None)
            .files()
            .into_iter()
            .chain([
                (default(AUTHORITATIVE), six.module_json(0)),
                (default(MIRROR), six.module_json(0)),
            ])
            .collect(),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, mixed).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::MixedVersioning { .. },
            ..
        }
    ));

    // The mirror left behind.
    let unmirrored = commit(
        &ctx,
        &repo,
        vec![root],
        six.files()
            .into_iter()
            .filter(|(path, _)| *path != default(MIRROR))
            .chain([(default(MIRROR), Generated::new(Some("6")).module_json(0))])
            .collect(),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, unmirrored).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::MirrorMismatch { .. },
            ..
        }
    ));

    // Revisions that do not fit in a signed 32-bit integer, or are not
    // integers, including the next one after the largest.
    for value in ["2147483648", "9223372036854775808", "2.147483647e9", "6.0"] {
        let overflow = generate(
            &ctx,
            &repo,
            root,
            &Generated::new(Some(value)).with(0, 0, "x"),
        )
        .await?;
        assert!(
            matches!(
                guard_error(land(&ctx, &repo, &hooks, overflow).await),
                SourceRevisionGuardError::InvalidCandidate {
                    error: SnapshotError::InvalidRevision { .. },
                    ..
                }
            ),
            "{value} must be rejected"
        );
    }

    // A string revision.
    let string = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("\"6\"")).with(0, 0, "x"),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, string).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::InvalidRevision { .. },
            ..
        }
    ));
    Ok(())
}

#[mononoke::fbinit_test]
async fn guard_accepts_the_largest_revision_and_rejects_its_successor(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("2147483646")))).await?;

    let max = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("2147483647")).with(0, 0, "x"),
    )
    .await?;
    land(&ctx, &repo, &hooks, max).await?;

    let head = master(&ctx, &repo).await?;
    let change = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("2147483647")).with(0, 0, "y"),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, change).await),
        TransitionError::RevisionOverflow {
            parent_revision: SourceRevision::MAX,
        }
    );

    // At the largest revision, a no-op regeneration still lands.
    let no_op = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("2147483647")).with(0, 0, "x"),
    )
    .await?;
    land(&ctx, &repo, &hooks, no_op).await?;

    // Moving past it to the next integer is not a revision at all.
    let past = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("2147483648")).with(0, 0, "y"),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, past).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::InvalidRevision { .. },
            ..
        }
    ));
    Ok(())
}

/// Two landings generated from the same parent both claim revision 6, for
/// different modules. Merge resolution combines the second with the first
/// without a conflict, since both changed the revision lines the same way;
/// only the guard can see that the combined content needs revision 7.
#[mononoke::fbinit_test]
async fn guard_rejects_the_second_landing_of_the_same_next_revision(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let first = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let second = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(2, 2, "y"),
    )
    .await?;

    // Without the guard the second lands, merged, still at revision 6.
    let unguarded: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let unguarded_root = init_master(&ctx, &unguarded, Some(&Generated::new(Some("5")))).await?;
    let unguarded_first = generate(
        &ctx,
        &unguarded,
        unguarded_root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let unguarded_second = generate(
        &ctx,
        &unguarded,
        unguarded_root,
        &Generated::new(Some("6")).with(2, 2, "y"),
    )
    .await?;
    land(&ctx, &unguarded, &[], unguarded_first).await?;
    let merged = land(&ctx, &unguarded, &[], unguarded_second).await?;
    assert!(merged.merge_resolved_paths.is_some());
    let files = list_working_copy_utf8(&ctx, &unguarded, merged.head).await?;
    let onboarding = &files[&NonRootMPath::new(format!("{AUTHORITATIVE}/onboarding.json"))?];
    assert!(onboarding.contains("\"source_revision\": 6") && onboarding.contains("\"y\""));
    assert!(
        generated_at(&ctx, &unguarded, merged.head)
            .await?
            .contains("\"x\"")
    );

    land(&ctx, &repo, &hooks, first).await?;
    let landed = master(&ctx, &repo).await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, second).await),
        TransitionError::DuplicateRevision {
            parent_revision: revision(6),
            expected: revision(7),
        }
    );
    assert_eq!(
        master(&ctx, &repo).await?,
        landed,
        "the bookmark did not move"
    );

    // Regenerated on the new parent, it lands.
    let regenerated = generate(
        &ctx,
        &repo,
        landed,
        &Generated::new(Some("7")).with(0, 0, "x").with(2, 2, "y"),
    )
    .await?;
    land(&ctx, &repo, &hooks, regenerated).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn guard_rejects_a_duplicate_revision_with_other_content(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("6")))).await?;

    let same = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(1, 1, "z"),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, same).await),
        TransitionError::DuplicateRevision {
            parent_revision: revision(6),
            expected: revision(7),
        }
    );
    let stale = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("5")).with(1, 1, "z"),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, stale).await),
        TransitionError::StaleRevision {
            parent_revision: revision(6),
            revision: revision(5),
            expected: revision(7),
        }
    );
    let gap = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("8")).with(1, 1, "z"),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, gap).await),
        TransitionError::RevisionGap {
            parent_revision: revision(6),
            revision: revision(8),
            expected: revision(7),
        }
    );
    Ok(())
}

async fn load_stack(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    ids: [ChangesetId; 2],
) -> Result<HashSet<BonsaiChangeset>> {
    let commits =
        try_join_all(ids.map(|id| async move { id.load(ctx, repo.repo_blobstore()).await }))
            .await?;
    Ok(commits.into_iter().collect())
}

/// Each commit of a stack is checked against its own rebased parent.
#[mononoke::fbinit_test]
async fn guard_checks_each_commit_of_a_stack_against_its_parent(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let six = Generated::new(Some("6")).with(0, 0, "x");

    let bottom = generate(&ctx, &repo, root, &six).await?;
    let good_top = generate(
        &ctx,
        &repo,
        bottom,
        &Generated::new(Some("7")).with(0, 0, "y"),
    )
    .await?;
    let bad_top = generate(
        &ctx,
        &repo,
        bottom,
        &Generated::new(Some("6")).with(0, 0, "y"),
    )
    .await?;

    let flags = PushrebaseFlags::default();
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &flags,
        &master_bookmark(),
        &load_stack(&ctx, &repo, [bottom, bad_top]).await?,
        &hooks,
        None,
    )
    .await;
    assert_eq!(
        transition_error(result),
        TransitionError::DuplicateRevision {
            parent_revision: revision(6),
            expected: revision(7),
        }
    );
    assert_eq!(master(&ctx, &repo).await?, root);

    do_pushrebase_bonsai(
        &ctx,
        &repo,
        &flags,
        &master_bookmark(),
        &load_stack(&ctx, &repo, [bottom, good_top]).await?,
        &hooks,
        None,
    )
    .await?;
    Ok(())
}

/// A hook that lands `competitor` the first time pushrebase enters its
/// critical section, so that attempt's bookmark swap fails.
struct LandOnce {
    repo: PushrebaseTestRepo,
    competitor: ChangesetId,
    landed: Arc<AtomicBool>,
}

#[async_trait]
impl PushrebaseHook for LandOnce {
    async fn in_critical_section(
        &self,
        ctx: &CoreContext,
        _old_bookmark_value: Option<ChangesetId>,
    ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
        if !self.landed.swap(true, Ordering::SeqCst) {
            bookmark(ctx, &self.repo, master_bookmark())
                .set_to(self.competitor)
                .await?;
        }
        Ok(Box::new(SleepHook))
    }
}

/// The first attempt checks against the old bookmark value and passes; its
/// swap fails because the competitor landed meanwhile, and the retry checks
/// the commit again on top of the competitor.
#[mononoke::fbinit_test]
async fn guard_checks_again_when_the_bookmark_moves(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let competitor = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let pushed = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(2, 2, "y"),
    )
    .await?;
    let landed = Arc::new(AtomicBool::new(false));
    // Derived up front, so the first attempt gets as far as the swap.
    derive(&ctx, &repo, &[root, competitor]).await?;
    let hooks: Vec<Box<dyn PushrebaseHook>> = vec![
        Box::new(LandOnce {
            repo: repo.clone(),
            competitor,
            landed: landed.clone(),
        }),
        SourceRevisionPushrebaseHook::try_new(&guard_config()?, &repo)?,
    ];

    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, pushed).await),
        TransitionError::DuplicateRevision {
            parent_revision: revision(6),
            expected: revision(7),
        }
    );
    assert!(landed.load(Ordering::SeqCst));
    assert_eq!(master(&ctx, &repo).await?, competitor);
    Ok(())
}

async fn queued(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    commits: &[ChangesetId],
) -> Result<(
    QueuedPushrebaseRequest,
    oneshot::Receiver<Result<PushrebaseOutcome, SharedError<PushrebaseError>>>,
)> {
    let flags = PushrebaseFlags::default();
    let pushed: HashSet<_> = try_join_all(
        commits
            .iter()
            .map(|id| async move { id.load(ctx, repo.repo_blobstore()).await }),
    )
    .await?
    .into_iter()
    .collect();
    let mut stack =
        index_pushrebase_request(ctx, repo, &flags, &master_bookmark(), &pushed).await?;
    stack
        .precompute(ctx, repo, &flags, &master_bookmark())
        .await?;
    let (tx, rx) = oneshot::channel();
    Ok((queued_pushrebase_request(ctx, stack, tx), rx))
}

/// Two requests in one batch claim revision 6. The second, chained on the
/// first inside the batch, is rejected against the first. A batch moves the
/// bookmark once, so the rejection fails the whole batch, as any hook error
/// does: neither lands and neither is requeued. Landed on its own, the first
/// succeeds.
#[mononoke::fbinit_test]
async fn guard_rejects_a_batch_whose_requests_claim_the_same_revision(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let flags = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..Default::default()
    };
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let first = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let second = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(2, 2, "y"),
    )
    .await?;
    let (first_request, first_rx) = queued(&ctx, &repo, &[first]).await?;
    let (second_request, second_rx) = queued(&ctx, &repo, &[second]).await?;
    derive(&ctx, &repo, &[root]).await?;

    let requeued = do_batched_pushrebase(
        &ctx,
        &repo,
        &flags,
        &master_bookmark(),
        vec![first_request, second_request],
        &hooks,
    )
    .await;
    assert!(requeued.is_empty(), "nothing is requeued");
    assert_eq!(master(&ctx, &repo).await?, root, "nothing landed");
    for rx in [first_rx, second_rx] {
        let rejection = rx.await?.expect_err("the whole batch is rejected");
        assert!(
            rejection.to_string().contains("source_revision 6"),
            "unexpected rejection: {rejection}"
        );
    }

    let (first_request, first_rx) = queued(&ctx, &repo, &[first]).await?;
    assert!(
        do_batched_pushrebase(
            &ctx,
            &repo,
            &flags,
            &master_bookmark(),
            vec![first_request],
            &hooks
        )
        .await
        .is_empty()
    );
    let outcome = first_rx.await??;
    assert_eq!(master(&ctx, &repo).await?, outcome.head);
    assert!(
        generated_at(&ctx, &repo, outcome.head)
            .await?
            .contains("\"x\"")
    );
    Ok(())
}

/// A batch of a two-commit stack taking revisions 6 and 7 and an unrelated
/// request lands as one: each commit is checked against its own parent.
#[mononoke::fbinit_test]
async fn guard_accepts_consecutive_revisions_in_one_batch(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let flags = PushrebaseFlags::default();
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let six = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let seven = generate(&ctx, &repo, six, &Generated::new(Some("7")).with(0, 0, "y")).await?;
    let unrelated = commit(&ctx, &repo, vec![root], vec![("x".into(), "x".into())]).await?;
    let (stack_request, stack_rx) = queued(&ctx, &repo, &[six, seven]).await?;
    let (unrelated_request, unrelated_rx) = queued(&ctx, &repo, &[unrelated]).await?;
    derive(&ctx, &repo, &[root]).await?;

    assert!(
        do_batched_pushrebase(
            &ctx,
            &repo,
            &flags,
            &master_bookmark(),
            vec![unrelated_request, stack_request],
            &hooks,
        )
        .await
        .is_empty()
    );
    let stack = stack_rx.await??;
    let unrelated = unrelated_rx.await??;
    assert_eq!(stack.log_id, unrelated.log_id, "one bookmark move");
    let landed = generated_at(&ctx, &repo, stack.head).await?;
    assert!(landed.contains("\"y\"") && landed.contains("\"source_revision\": 7"));
    Ok(())
}

/// The first request of a batch adds an unversioned file next to versioned
/// ones; the second, valid on its own, is chained on it. Commits are checked
/// parents first, so the rejection names the first request's file. The
/// whole batch fails.
#[mononoke::fbinit_test]
async fn guard_blames_the_request_that_broke_the_snapshot(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let flags = PushrebaseFlags::default();
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let unversioned = "{\n    \"strings\": []\n}\n";
    let broken = commit(
        &ctx,
        &repo,
        vec![root],
        [AUTHORITATIVE, MIRROR]
            .map(|dir| (format!("{dir}/unversioned.json"), unversioned.to_string()))
            .to_vec(),
    )
    .await?;
    let valid = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let (broken_request, broken_rx) = queued(&ctx, &repo, &[broken]).await?;
    let (valid_request, valid_rx) = queued(&ctx, &repo, &[valid]).await?;
    derive(&ctx, &repo, &[root]).await?;

    let requeued = do_batched_pushrebase(
        &ctx,
        &repo,
        &flags,
        &master_bookmark(),
        vec![broken_request, valid_request],
        &hooks,
    )
    .await;
    assert!(requeued.is_empty());
    for rx in [broken_rx, valid_rx] {
        let rejection = rx
            .await?
            .expect_err("the whole batch is rejected")
            .to_string();
        // `InvalidCandidate` on the broken commit, not `InvalidParent` on
        // the valid one that is chained on it.
        assert!(
            rejection.contains("are invalid")
                && !rejection.contains("lands on")
                && rejection.contains("unversioned.json has none"),
            "unexpected rejection: {rejection}"
        );
    }
    assert_eq!(master(&ctx, &repo).await?, root);
    Ok(())
}

/// A new module lands with every other file bumped; a removed module needs
/// the bump too.
#[mononoke::fbinit_test]
async fn guard_sees_added_and_removed_modules(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let new_module = |revision: &str| {
        let json = format!("{{\n    \"source_revision\": {revision},\n    \"strings\": []\n}}\n");
        [AUTHORITATIVE, MIRROR].map(|dir| (format!("{dir}/new_module.json"), json.clone()))
    };

    // The new module without its mirror.
    let unmirrored = commit(
        &ctx,
        &repo,
        vec![root],
        Generated::new(Some("6"))
            .files()
            .into_iter()
            .chain(new_module("6").into_iter().take(1))
            .collect(),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, unmirrored).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::MirrorMissingFile { .. },
            ..
        }
    ));

    // The new module, with the old files only bumped.
    let added = commit(
        &ctx,
        &repo,
        vec![root],
        Generated::new(Some("6"))
            .files()
            .into_iter()
            .chain(new_module("6"))
            .collect(),
    )
    .await?;
    land(&ctx, &repo, &hooks, added).await?;

    // Another new module, with the old files left at their revision.
    let head = master(&ctx, &repo).await?;
    let stale_added = commit(
        &ctx,
        &repo,
        vec![head],
        new_module("7")
            .map(|(path, json)| (path.replace("new_module", "newer_module"), json))
            .to_vec(),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, stale_added).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::RevisionMismatch { .. },
            ..
        }
    ));

    // Removing it without a bump.
    fn remove<'a>(
        commit: CreateCommitContext<'a, PushrebaseTestRepo>,
    ) -> CreateCommitContext<'a, PushrebaseTestRepo> {
        [AUTHORITATIVE, MIRROR].iter().fold(commit, |commit, dir| {
            commit.delete_file(format!("{dir}/new_module.json").as_str())
        })
    }
    let removed = remove(CreateCommitContext::new(&ctx, &repo, vec![head]))
        .commit()
        .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, removed).await),
        TransitionError::DuplicateRevision {
            parent_revision: revision(6),
            expected: revision(7),
        }
    );

    // Removing it with the bump.
    let removed = Generated::new(Some("7"))
        .files()
        .into_iter()
        .fold(
            remove(CreateCommitContext::new(&ctx, &repo, vec![head])),
            |commit, (path, content)| commit.add_file(path.as_str(), content),
        )
        .commit()
        .await?;
    land(&ctx, &repo, &hooks, removed).await?;
    Ok(())
}

/// Nothing but immediate `*.json` files may be written under a guarded
/// directory, and no file may replace it.
#[mononoke::fbinit_test]
async fn guard_rejects_files_that_could_hide_generated_files(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    for path in [
        format!("{AUTHORITATIVE}/nested/default.json"),
        format!("{MIRROR}/README"),
        AUTHORITATIVE.to_string(),
        "www/flib/whatsapp".to_string(),
    ] {
        let pushed = commit(&ctx, &repo, vec![root], vec![(path.clone(), "{}".into())]).await?;
        assert!(
            matches!(
                guard_error(land(&ctx, &repo, &hooks, pushed).await),
                SourceRevisionGuardError::InvalidChange {
                    error: SnapshotError::UnexpectedPath { .. },
                    ..
                }
            ),
            "{path} must be rejected"
        );
    }

    // A sibling directory is not guarded.
    let sibling = commit(
        &ctx,
        &repo,
        vec![root],
        vec![(format!("{AUTHORITATIVE}-notes/README"), "x".into())],
    )
    .await?;
    land(&ctx, &repo, &hooks, sibling).await?;
    Ok(())
}

async fn land_onto(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    hooks: &[Box<dyn PushrebaseHook>],
    onto: &str,
    pushed: &[ChangesetId],
) -> Result<PushrebaseOutcome, PushrebaseError> {
    let pushed: HashSet<_> = try_join_all(
        pushed
            .iter()
            .map(|id| async move { id.load(ctx, repo.repo_blobstore()).await }),
    )
    .await
    .map_err(Error::from)?
    .into_iter()
    .collect();
    do_pushrebase_bonsai(
        ctx,
        repo,
        &PushrebaseFlags::default(),
        &BookmarkKey::new(onto)?,
        &pushed,
        hooks,
        None,
    )
    .await
}

/// The hook reads through the repo it was built from. Built from an empty
/// repo, it would fail any read for this one, so an unrelated landing that
/// succeeds did no reads at all.
#[mononoke::fbinit_test]
async fn guard_does_no_reads_for_unrelated_commits(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let elsewhere: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = vec![SourceRevisionPushrebaseHook::try_new(
        &guard_config()?,
        &elsewhere,
    )?];
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let unrelated = commit(&ctx, &repo, vec![root], vec![("x".into(), "x".into())]).await?;
    land(&ctx, &repo, &hooks, unrelated).await?;

    let head = master(&ctx, &repo).await?;
    let guarded = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    assert!(
        land(&ctx, &repo, &hooks, guarded).await.is_err(),
        "a guarded change reads, and the reads go to the wrong repo"
    );
    Ok(())
}

/// Records, each time pushrebase enters its critical section, whether the
/// bookmark's content manifest is derived at that point.
struct ObserveDerivation {
    repo: PushrebaseTestRepo,
    derived_at_start: Arc<std::sync::Mutex<Vec<bool>>>,
}

#[async_trait]
impl PushrebaseHook for ObserveDerivation {
    async fn in_critical_section(
        &self,
        ctx: &CoreContext,
        old_bookmark_value: Option<ChangesetId>,
    ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
        let onto = old_bookmark_value.context("the bookmark exists")?;
        let derived = self
            .repo
            .repo_derived_data()
            .fetch_derived::<RootContentManifestId>(ctx, onto)
            .await?
            .is_some();
        self.derived_at_start
            .lock()
            .expect("the lock is not poisoned")
            .push(derived);
        Ok(Box::new(SleepHook))
    }
}

/// Nothing is derived in the critical section. The bookmark's manifest is
/// missing when the first attempt starts; the guard asks for preparation,
/// pushrebase derives it between attempts, and the second attempt starts
/// with it derived and lands.
#[mononoke::fbinit_test]
async fn guard_derives_only_outside_the_critical_section(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let derived_at_start = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hooks: Vec<Box<dyn PushrebaseHook>> = vec![
        Box::new(ObserveDerivation {
            repo: repo.clone(),
            derived_at_start: derived_at_start.clone(),
        }),
        SourceRevisionPushrebaseHook::try_new(&guard_config()?, &repo)?,
    ];

    let guarded = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let outcome = land(&ctx, &repo, &hooks, guarded).await?;
    assert_eq!(
        outcome.retry_num.0, 1,
        "one attempt to prepare, one to land"
    );
    assert_eq!(
        *derived_at_start.lock().expect("the lock is not poisoned"),
        vec![false, true],
        "derived between the attempts, not during the first"
    );
    Ok(())
}

/// The batch path prepares after its critical section and requeues the
/// whole batch, as after a failed swap; the requeued batch lands.
#[mononoke::fbinit_test]
async fn guard_prepares_between_batches(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let flags = PushrebaseFlags::default();
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let guarded = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let unrelated = commit(&ctx, &repo, vec![root], vec![("x".into(), "x".into())]).await?;
    let (guarded_request, guarded_rx) = queued(&ctx, &repo, &[guarded]).await?;
    let (unrelated_request, unrelated_rx) = queued(&ctx, &repo, &[unrelated]).await?;

    let requeued = do_batched_pushrebase(
        &ctx,
        &repo,
        &flags,
        &master_bookmark(),
        vec![guarded_request, unrelated_request],
        &hooks,
    )
    .await;
    assert_eq!(requeued.len(), 2, "the whole batch is requeued");
    assert_eq!(master(&ctx, &repo).await?, root, "nothing landed");
    assert!(
        repo.repo_derived_data()
            .fetch_derived::<RootContentManifestId>(&ctx, root)
            .await?
            .is_some(),
        "prepared after the critical section"
    );

    assert!(
        do_batched_pushrebase(&ctx, &repo, &flags, &master_bookmark(), requeued, &hooks)
            .await
            .is_empty()
    );
    assert_eq!(guarded_rx.await??.retry_num.0, 1);
    unrelated_rx.await??;
    Ok(())
}

/// A merge that changes the generated files is rejected; one that leaves
/// them alone lands.
#[mononoke::fbinit_test]
async fn guard_rejects_merges_that_change_generated_files(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let moved = commit(&ctx, &repo, vec![root], vec![("moved".into(), "x".into())]).await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(moved)
        .await?;

    let side = commit(&ctx, &repo, vec![root], vec![("side".into(), "x".into())]).await?;
    let generated_merge = commit(
        &ctx,
        &repo,
        vec![root, side],
        Generated::new(Some("6")).with(0, 0, "x").files(),
    )
    .await?;
    assert!(matches!(
        guard_error(land_onto(&ctx, &repo, &hooks, "master", &[side, generated_merge]).await),
        SourceRevisionGuardError::MergeChangesGeneratedFiles { .. }
    ));

    let plain_merge = commit(
        &ctx,
        &repo,
        vec![root, side],
        vec![("merged".into(), "x".into())],
    )
    .await?;
    land_onto(&ctx, &repo, &hooks, "master", &[side, plain_merge]).await?;
    Ok(())
}

/// Creating a bookmark checks the stack against the commit it starts from.
#[mononoke::fbinit_test]
async fn guard_checks_bookmark_creation(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;

    let duplicate = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("5")).with(0, 0, "x"),
    )
    .await?;
    assert_eq!(
        transition_error(land_onto(&ctx, &repo, &hooks, "created", &[duplicate]).await),
        TransitionError::DuplicateRevision {
            parent_revision: revision(5),
            expected: revision(6),
        }
    );
    assert!(
        resolve_cs_id(&ctx, &repo, "created").await.is_err(),
        "the bookmark was not created"
    );

    let next = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let outcome = land_onto(&ctx, &repo, &hooks, "created", &[next]).await?;
    assert_eq!(resolve_cs_id(&ctx, &repo, "created").await?, outcome.head);
    Ok(())
}

/// `max_files` holds per directory, `max_file_bytes` per file, and
/// `max_total_bytes` across the snapshot.
#[mononoke::fbinit_test]
async fn guard_enforces_its_limits(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let six = Generated::new(Some("6")).with(0, 0, "x");
    // Revision 5's files are the largest the test lands.
    let five = Generated::new(Some("5")).files();
    let largest = five.iter().map(|(_, json)| json.len()).max().unwrap_or(0) as u64;
    let total: u64 = five.iter().map(|(_, json)| json.len() as u64).sum();
    let with_limits = |max_files, max_file_bytes, max_total_bytes| -> Result<_> {
        Ok(vec![SourceRevisionPushrebaseHook::try_new(
            &SourceRevisionGuardConfig {
                max_files,
                max_file_bytes,
                max_total_bytes,
                ..guard_config()?
            },
            &repo,
        )?])
    };

    // At the limits, it lands.
    let at_limits = generate(&ctx, &repo, root, &six).await?;
    land(
        &ctx,
        &repo,
        &with_limits(MODULES.len(), largest, total)?,
        at_limits,
    )
    .await?;
    let head = master(&ctx, &repo).await?;
    let seven = Generated::new(Some("7")).with(0, 0, "y");

    // One more module than `max_files`, in each directory.
    let extra = "{\n    \"source_revision\": 7,\n    \"strings\": []\n}\n".to_string();
    let too_many = commit(
        &ctx,
        &repo,
        vec![head],
        seven
            .files()
            .into_iter()
            .chain([AUTHORITATIVE, MIRROR].map(|dir| (format!("{dir}/extra.json"), extra.clone())))
            .collect(),
    )
    .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &with_limits(MODULES.len(), 1 << 20, 1 << 22)?, too_many).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::TooManyFiles { limit, .. },
            ..
        } if limit == MODULES.len()
    ));

    // One file over `max_file_bytes`.
    let padded =
        Generated::new(Some("7")).with(0, 0, "a-string-long-enough-to-exceed-the-largest-file");
    let too_large = generate(&ctx, &repo, head, &padded).await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &with_limits(64, largest, 1 << 22)?, too_large).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::FileTooLarge { .. },
            ..
        }
    ));

    // Every file within `max_file_bytes`, the snapshot over `max_total_bytes`.
    assert!(matches!(
        guard_error(land(&ctx, &repo, &with_limits(64, 1 << 20, total)?, too_large).await),
        SourceRevisionGuardError::InvalidCandidate {
            error: SnapshotError::SnapshotTooLarge { .. },
            ..
        }
    ));
    Ok(())
}

/// A stack that adds one module per commit stays within the per-commit
/// change limit, but its snapshot grows; the first commit whose snapshot
/// exceeds `max_files` is rejected before any descendant builds on it.
#[mononoke::fbinit_test]
async fn guard_bounds_snapshots_that_grow_along_a_stack(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let max_files = MODULES.len() + 1;
    let hooks = vec![SourceRevisionPushrebaseHook::try_new(
        &SourceRevisionGuardConfig {
            max_files,
            ..guard_config()?
        },
        &repo,
    )?];
    let root = init_master(&ctx, &repo, Some(&Generated::new(None))).await?;

    let module = "{\n    \"strings\": []\n}\n";
    let mut stack = Vec::new();
    let mut head = root;
    for index in 0..3 {
        head = commit(
            &ctx,
            &repo,
            vec![head],
            [AUTHORITATIVE, MIRROR]
                .map(|dir| (format!("{dir}/added_{index}.json"), module.to_string()))
                .to_vec(),
        )
        .await?;
        stack.push(head);
    }
    let result = land_onto(&ctx, &repo, &hooks, "master", &stack).await;
    let error = guard_error(result);
    assert!(
        matches!(
            &error,
            SourceRevisionGuardError::InvalidCandidate {
                error: SnapshotError::TooManyFiles { limit, .. },
                ..
            } if *limit == max_files
        ),
        "unexpected rejection: {error}"
    );
    // The second commit is the first with five modules in a directory.
    assert!(error.to_string().contains("more than 4 entries"));
    assert_eq!(master(&ctx, &repo).await?, root);
    Ok(())
}

/// A commit on `parent` whose only change is a deep copy of `from_path` at
/// `from` to `path`, the way `sl subtree copy` records one.
async fn subtree_copy(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    parent: ChangesetId,
    path: &str,
    from_path: &str,
    from: ChangesetId,
) -> Result<ChangesetId> {
    let bcs = BonsaiChangesetMut {
        parents: vec![parent],
        author: "author".to_string(),
        author_date: DateTime::from_timestamp(0, 0)?,
        message: format!("subtree copy {from_path} to {path}"),
        subtree_changes: [(
            MPath::new(path)?,
            SubtreeChange::deep_copy(MPath::new(from_path)?, from),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    }
    .freeze()?;
    let id = bcs.get_changeset_id();
    changesets_creation::save_changesets(ctx, repo, vec![bcs]).await?;
    Ok(id)
}

/// Revision 5 at `root`, revision 6 on master.
async fn init_six(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<(ChangesetId, ChangesetId)> {
    let root = init_master(ctx, repo, Some(&Generated::new(Some("5")))).await?;
    let six = generate(ctx, repo, root, &Generated::new(Some("6")).with(0, 0, "x")).await?;
    bookmark(ctx, repo, master_bookmark()).set_to(six).await?;
    Ok((root, six))
}

/// A subtree operation at, above or below a guarded directory would change
/// its files without listing them, so it is rejected before anything else.
#[mononoke::fbinit_test]
async fn guard_rejects_subtree_changes_to_generated_files(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let (root, six) = init_six(&ctx, &repo).await?;

    for (path, from_path) in [
        // Back to revision 5's files.
        (AUTHORITATIVE.to_string(), AUTHORITATIVE.to_string()),
        (MIRROR.to_string(), MIRROR.to_string()),
        // Replacing an ancestor of the directory.
        (
            "www/flib/whatsapp/mms".to_string(),
            "www/flib/whatsapp/mms".to_string(),
        ),
        // Below the directory.
        (format!("{AUTHORITATIVE}/nested"), AUTHORITATIVE.to_string()),
    ] {
        let pushed = subtree_copy(&ctx, &repo, six, &path, &from_path, root).await?;
        let error = guard_error(land(&ctx, &repo, &hooks, pushed).await);
        assert!(
            matches!(
                &error,
                SourceRevisionGuardError::InvalidChange {
                    changeset,
                    error: SnapshotError::SubtreeChange { path: rejected },
                } if *changeset == pushed && rejected.to_string() == path
            ),
            "{path}: unexpected rejection: {error}"
        );
    }
    assert_eq!(master(&ctx, &repo).await?, six);
    Ok(())
}

/// A subtree copy next to a guarded directory, even from it, does not change
/// it: it lands, and without reads (the hook is built over an empty repo).
#[mononoke::fbinit_test]
async fn guard_lets_unrelated_subtree_changes_land_without_reads(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let elsewhere: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = vec![SourceRevisionPushrebaseHook::try_new(
        &guard_config()?,
        &elsewhere,
    )?];
    let (root, six) = init_six(&ctx, &repo).await?;

    let sibling = subtree_copy(
        &ctx,
        &repo,
        six,
        "www/flib/whatsapp/mms/dynamic_strings/generated_copy",
        AUTHORITATIVE,
        root,
    )
    .await?;
    land(&ctx, &repo, &hooks, sibling).await?;
    Ok(())
}

/// In a stack, a subtree copy of old files followed by a regenerated commit
/// is rejected at the copy, not checked against a snapshot that misses it.
#[mononoke::fbinit_test]
async fn guard_rejects_a_stack_with_a_subtree_change_below_its_regeneration(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    let (root, six) = init_six(&ctx, &repo).await?;

    let copy = subtree_copy(&ctx, &repo, six, AUTHORITATIVE, AUTHORITATIVE, root).await?;
    let regenerated = generate(
        &ctx,
        &repo,
        copy,
        &Generated::new(Some("7")).with(0, 0, "y"),
    )
    .await?;
    let error = guard_error(land_onto(&ctx, &repo, &hooks, "master", &[copy, regenerated]).await);
    assert!(
        matches!(
            &error,
            SourceRevisionGuardError::InvalidChange {
                changeset,
                error: SnapshotError::SubtreeChange { .. },
            } if *changeset == copy
        ),
        "unexpected rejection: {error}"
    );
    assert_eq!(master(&ctx, &repo).await?, six);
    Ok(())
}

/// The stats tell a content rejection from a missing manifest and from a
/// failed read.
#[mononoke::fbinit_test]
async fn guard_outcomes_tell_rejections_from_preparation_and_read_failures(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let outcome = |result: Result<PushrebaseOutcome, PushrebaseError>| match result {
        Ok(_) => GuardOutcome::Passed,
        Err(PushrebaseError::Error(error)) => GuardOutcome::of(&Err(error)),
        Err(other) => panic!("unexpected pushrebase error: {other}"),
    };

    // Preparation, then success: the first attempt needs the manifest.
    let next = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let landed = land(&ctx, &repo, &guard(&repo)?, next).await?;
    assert_eq!(landed.retry_num.0, 1, "one preparation, then success");

    // A content rejection.
    let head = master(&ctx, &repo).await?;
    let duplicate = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("6")).with(0, 0, "y"),
    )
    .await?;
    assert_eq!(
        outcome(land(&ctx, &repo, &guard(&repo)?, duplicate).await),
        GuardOutcome::Rejected
    );

    // A read failure: the hook finds the head's derived manifest, but reads
    // from an empty blobstore.
    derive(&ctx, &repo, &[head]).await?;
    let unreadable = vec![SourceRevisionPushrebaseHook::try_new(
        &guard_config()?,
        &unreadable(&repo, fb).await?,
    )?];
    let next = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("7")).with(0, 0, "y"),
    )
    .await?;
    assert_eq!(
        outcome(land(&ctx, &repo, &unreadable, next).await),
        GuardOutcome::ReadFailed
    );
    Ok(())
}

/// Zero limits would reject every landing that touches the directories, so
/// they fail construction; the largest limits do not overflow.
#[mononoke::fbinit_test]
async fn guard_limits_must_be_positive_and_may_be_huge(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let config = guard_config()?;
    for zero in [
        SourceRevisionGuardConfig {
            max_files: 0,
            ..config.clone()
        },
        SourceRevisionGuardConfig {
            max_file_bytes: 0,
            ..config.clone()
        },
        SourceRevisionGuardConfig {
            max_total_bytes: 0,
            ..config.clone()
        },
    ] {
        assert!(SourceRevisionPushrebaseHook::try_new(&zero, &repo).is_err());
    }

    let huge = vec![SourceRevisionPushrebaseHook::try_new(
        &SourceRevisionGuardConfig {
            max_files: usize::MAX,
            max_file_bytes: u64::MAX,
            max_total_bytes: u64::MAX,
            ..config
        },
        &repo,
    )?];
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let next = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    land(&ctx, &repo, &huge, next).await?;
    Ok(())
}

/// One commit may change up to `2 * max_files` paths under a directory:
/// every entry replaced. One more is rejected.
#[mononoke::fbinit_test]
async fn guard_limits_changes_per_directory(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = vec![SourceRevisionPushrebaseHook::try_new(
        &SourceRevisionGuardConfig {
            max_files: MODULES.len(),
            ..guard_config()?
        },
        &repo,
    )?];
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    fn renamed<'a>(
        commit: CreateCommitContext<'a, PushrebaseTestRepo>,
    ) -> CreateCommitContext<'a, PushrebaseTestRepo> {
        Generated::new(Some("6"))
            .files()
            .into_iter()
            .fold(commit, |commit, (path, json)| {
                commit
                    .delete_file(path.as_str())
                    .add_file(path.replace(".json", "_renamed.json").as_str(), json)
            })
    }

    // One more change, a deletion of a path that is not a guarded file.
    let over = renamed(CreateCommitContext::new(&ctx, &repo, vec![root]))
        .delete_file(format!("{AUTHORITATIVE}/README").as_str())
        .commit()
        .await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, over).await),
        SourceRevisionGuardError::InvalidChange {
            error: SnapshotError::TooManyChanges { limit, .. },
            ..
        } if limit == 2 * MODULES.len()
    ));

    // Three deletions and three additions in each directory: at the limit.
    let at_limit = renamed(CreateCommitContext::new(&ctx, &repo, vec![root]))
        .commit()
        .await?;
    land(&ctx, &repo, &hooks, at_limit).await?;
    Ok(())
}

/// A guarded directory holds only guarded files. A parent with anything
/// else there is rejected, so `max_files` means the same in the parent and
/// the candidate; a commit that only deletes such an entry lands, and the
/// generated change after it lands too.
#[mononoke::fbinit_test]
async fn guard_requires_directories_to_hold_only_generated_files(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hooks = guard(&repo)?;
    // Landed before the guard checked.
    let landed_with = |files: Vec<(String, String)>| {
        let ctx = &ctx;
        let repo = &repo;
        async move {
            let root = commit(
                ctx,
                repo,
                vec![],
                Generated::new(Some("5"))
                    .files()
                    .into_iter()
                    .chain(files)
                    .collect(),
            )
            .await?;
            bookmark(ctx, repo, master_bookmark()).set_to(root).await?;
            Result::<_, Error>::Ok(root)
        }
    };

    for stray in [
        format!("{AUTHORITATIVE}/README"),
        format!("{MIRROR}/extra.json/old"),
    ] {
        let root = landed_with(vec![(stray.clone(), "x".into())]).await?;
        let next = generate(
            &ctx,
            &repo,
            root,
            &Generated::new(Some("6")).with(0, 0, "x"),
        )
        .await?;
        let error = guard_error(land(&ctx, &repo, &hooks, next).await);
        assert!(
            matches!(
                &error,
                SourceRevisionGuardError::InvalidParent {
                    error: SnapshotError::UnexpectedPath { .. },
                    ..
                }
            ),
            "{stray}: unexpected rejection: {error}"
        );

        // Deleting the stray entry is not a change to the generated files.
        let cleaned = CreateCommitContext::new(&ctx, &repo, vec![root])
            .delete_file(stray.as_str())
            .commit()
            .await?;
        land(&ctx, &repo, &hooks, cleaned).await?;
        let head = master(&ctx, &repo).await?;
        let next = generate(
            &ctx,
            &repo,
            head,
            &Generated::new(Some("6")).with(0, 0, "x"),
        )
        .await?;
        land(&ctx, &repo, &hooks, next).await?;
    }

    // A merge whose second parent brings a README in: pushrebase lists it in
    // the rebased merge, which is rejected as a write under the directory.
    let root = landed_with(vec![]).await?;
    let side = commit(
        &ctx,
        &repo,
        vec![root],
        vec![(format!("{AUTHORITATIVE}/README"), "x".into())],
    )
    .await?;
    derive(&ctx, &repo, &[root, side]).await?;
    let merge = commit(
        &ctx,
        &repo,
        vec![root, side],
        vec![("merged".into(), "x".into())],
    )
    .await?;
    let error = guard_error(land_onto(&ctx, &repo, &hooks, "master", &[merge]).await);
    assert!(
        matches!(
            &error,
            SourceRevisionGuardError::InvalidChange {
                error: SnapshotError::UnexpectedPath { path },
                ..
            } if path.to_string().ends_with("README")
        ),
        "unexpected rejection: {error}"
    );
    Ok(())
}

/// A hook that asks for preparation on its first attempt and counts the
/// preparation rounds pushrebase runs for it.
struct PrepareOnce {
    asked: Arc<AtomicBool>,
    prepared: Arc<AtomicUsize>,
}

#[async_trait]
impl PushrebaseHook for PrepareOnce {
    async fn prepare(&self, _ctx: &CoreContext, _changesets: &[ChangesetId]) -> Result<(), Error> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn in_critical_section(
        &self,
        _ctx: &CoreContext,
        _old_bookmark_value: Option<ChangesetId>,
    ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
        Ok(Box::new(AskOnce {
            asked: self.asked.clone(),
        }))
    }
}

struct AskOnce {
    asked: Arc<AtomicBool>,
}

#[async_trait]
impl PushrebaseCommitHook for AskOnce {
    async fn into_transaction_hook(
        self: Box<Self>,
        _ctx: &CoreContext,
        _changesets: &RebasedChangesets,
    ) -> Result<Box<dyn PushrebaseTransactionHook>, Error> {
        if self.asked.swap(true, Ordering::SeqCst) {
            Ok(Box::new(SleepHook))
        } else {
            Err(PreparationRequired { changesets: vec![] }.into())
        }
    }
}

/// The direct and the batched path each run exactly one preparation round
/// for one request to prepare, through `prepare_hooks`, which is also where
/// `preparations` is counted; neither treats it as a rejection.
#[mononoke::fbinit_test]
async fn preparation_rounds_are_counted_alike_on_both_paths(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, None).await?;
    let hook = || {
        let prepared = Arc::new(AtomicUsize::new(0));
        let hooks: Vec<Box<dyn PushrebaseHook>> = vec![Box::new(PrepareOnce {
            asked: Arc::new(AtomicBool::new(false)),
            prepared: prepared.clone(),
        })];
        (hooks, prepared)
    };

    // Direct: one preparation, then the retry lands.
    let (hooks, prepared) = hook();
    let direct = commit(&ctx, &repo, vec![root], vec![("direct".into(), "x".into())]).await?;
    let outcome = land(&ctx, &repo, &hooks, direct).await?;
    assert_eq!(outcome.retry_num.0, 1);
    assert_eq!(prepared.load(Ordering::SeqCst), 1);

    // Batched: one preparation, the whole batch requeued without a
    // response, then it lands.
    let (hooks, prepared) = hook();
    let head = master(&ctx, &repo).await?;
    let batched = commit(
        &ctx,
        &repo,
        vec![head],
        vec![("batched".into(), "x".into())],
    )
    .await?;
    let (request, rx) = queued(&ctx, &repo, &[batched]).await?;
    let flags = PushrebaseFlags::default();
    let requeued = do_batched_pushrebase(
        &ctx,
        &repo,
        &flags,
        &master_bookmark(),
        vec![request],
        &hooks,
    )
    .await;
    assert_eq!(requeued.len(), 1);
    assert_eq!(prepared.load(Ordering::SeqCst), 1);
    assert!(
        do_batched_pushrebase(&ctx, &repo, &flags, &master_bookmark(), requeued, &hooks)
            .await
            .is_empty()
    );
    assert_eq!(rx.await??.retry_num.0, 1);
    assert_eq!(prepared.load(Ordering::SeqCst), 1, "no second preparation");
    Ok(())
}

fn counted_guard(
    repo: &(impl RepoBlobstoreArc + RepoDerivedDataArc),
) -> Result<(Vec<Box<dyn PushrebaseHook>>, Arc<GuardCounters>)> {
    let (hook, counters) =
        SourceRevisionPushrebaseHook::try_new_with_counters(&guard_config()?, repo)?;
    Ok((vec![hook], counters))
}

/// `repo`'s derived data with an empty blobstore: a guard built on it finds
/// derived manifests, then fails to read them.
struct Unreadable {
    blobstore: Arc<RepoBlobstore>,
    derived_data: Arc<RepoDerivedData>,
}

impl facet::FacetArc<RepoBlobstore> for Unreadable {
    fn facet_arc(&self) -> Arc<RepoBlobstore> {
        self.blobstore.clone()
    }
}

impl facet::FacetRef<RepoBlobstore> for Unreadable {
    fn facet_ref(&self) -> &RepoBlobstore {
        &self.blobstore
    }
}

impl facet::FacetArc<RepoDerivedData> for Unreadable {
    fn facet_arc(&self) -> Arc<RepoDerivedData> {
        self.derived_data.clone()
    }
}

impl facet::FacetRef<RepoDerivedData> for Unreadable {
    fn facet_ref(&self) -> &RepoDerivedData {
        &self.derived_data
    }
}

async fn unreadable(repo: &PushrebaseTestRepo, fb: FacebookInit) -> Result<Unreadable> {
    let empty: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    Ok(Unreadable {
        blobstore: empty.repo_blobstore_arc(),
        derived_data: repo.repo_derived_data_arc(),
    })
}

/// Every outcome is counted once, in the phase that decides it: a layout
/// rejection while rebasing, a content rejection, a preparation or a failed
/// read before the swap. These are the counters the hook also adds to the
/// process stats, which tests cannot read.
#[mononoke::fbinit_test]
async fn guard_counts_each_outcome_once_in_its_phase(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (root, six) = init_six(&ctx, &repo).await?;
    derive(&ctx, &repo, &[six]).await?;
    let counts =
        |checked, rejected, rejected_layout, preparation_required, read_failed, prepared| {
            GuardCounts {
                checked,
                rejected,
                rejected_layout,
                preparation_required,
                read_failed,
                prepared,
            }
        };

    // Layout rejections while rebasing: subtree, stray path, too many
    // changes. Each is checked and rejected once, and never reaches the
    // transaction phase.
    let subtree = subtree_copy(&ctx, &repo, six, AUTHORITATIVE, AUTHORITATIVE, root).await?;
    let stray = commit(
        &ctx,
        &repo,
        vec![six],
        vec![(format!("{AUTHORITATIVE}/README"), "x".into())],
    )
    .await?;
    let many = commit(
        &ctx,
        &repo,
        vec![six],
        (0..=128)
            .map(|index| (format!("{AUTHORITATIVE}/m{index}.json"), "{}".into()))
            .collect(),
    )
    .await?;
    for pushed in [subtree, stray, many] {
        let (hooks, counters) = counted_guard(&repo)?;
        assert!(matches!(
            guard_error(land(&ctx, &repo, &hooks, pushed).await),
            SourceRevisionGuardError::InvalidChange { .. }
        ));
        assert_eq!(counters.counts(), counts(1, 1, 1, 0, 0, 0));
    }

    // A content rejection before the swap.
    let (hooks, counters) = counted_guard(&repo)?;
    let duplicate = generate(&ctx, &repo, six, &Generated::new(Some("6")).with(0, 0, "y")).await?;
    assert!(matches!(
        guard_error(land(&ctx, &repo, &hooks, duplicate).await),
        SourceRevisionGuardError::InvalidTransition { .. }
    ));
    assert_eq!(counters.counts(), counts(1, 1, 0, 0, 0, 0));

    // A preparation, then success: no rejection.
    let underived = commit(&ctx, &repo, vec![six], vec![("x".into(), "x".into())]).await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(underived)
        .await?;
    let (hooks, counters) = counted_guard(&repo)?;
    let next = generate(
        &ctx,
        &repo,
        underived,
        &Generated::new(Some("7")).with(0, 0, "y"),
    )
    .await?;
    land(&ctx, &repo, &hooks, next).await?;
    assert_eq!(counters.counts(), counts(2, 0, 0, 1, 0, 1));

    // A failed read: the hook finds the head's derived manifest, but reads
    // the manifest and files from an empty blobstore. No rejection.
    let head = master(&ctx, &repo).await?;
    derive(&ctx, &repo, &[head]).await?;
    let (hooks, counters) = counted_guard(&unreadable(&repo, fb).await?)?;
    let next = generate(
        &ctx,
        &repo,
        head,
        &Generated::new(Some("8")).with(0, 0, "z"),
    )
    .await?;
    assert!(land(&ctx, &repo, &hooks, next).await.is_err());
    assert_eq!(counters.counts(), counts(1, 0, 0, 0, 1, 0));

    // An unrelated commit: nothing.
    let (hooks, counters) = counted_guard(&repo)?;
    let unrelated = commit(&ctx, &repo, vec![head], vec![("y".into(), "y".into())]).await?;
    land(&ctx, &repo, &hooks, unrelated).await?;
    assert_eq!(counters.counts(), GuardCounts::default());
    Ok(())
}

/// Moves the bookmark to `target` the `attempt`-th time (from zero)
/// pushrebase enters its critical section, so that attempt's swap fails.
struct MoveOnAttempt {
    repo: PushrebaseTestRepo,
    target: ChangesetId,
    attempt: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl PushrebaseHook for MoveOnAttempt {
    /// Derives the target, so that the attempt after the lost swap does not
    /// need a preparation of its own.
    async fn prepare(&self, ctx: &CoreContext, _changesets: &[ChangesetId]) -> Result<(), Error> {
        derive(ctx, &self.repo, &[self.target]).await
    }

    async fn in_critical_section(
        &self,
        ctx: &CoreContext,
        _old_bookmark_value: Option<ChangesetId>,
    ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.attempt {
            bookmark(ctx, &self.repo, master_bookmark())
                .set_to(self.target)
                .await?;
        }
        Ok(Box::new(SleepHook))
    }
}

#[mononoke::test]
fn contention_retries_leave_out_preparation() {
    assert_eq!(contention_retries(0, 0), 0);
    assert_eq!(
        contention_retries(2, 1),
        1,
        "one swap failed, one preparation"
    );
    assert_eq!(contention_retries(1, 1), 0, "only a preparation");
    assert_eq!(contention_retries(1, 5), 0, "saturates");
}

/// A landing that needs one preparation and then loses one swap retries
/// twice: both count against the attempt budget, and only the lost swap is
/// contention.
#[mononoke::fbinit_test]
async fn preparation_and_a_failed_swap_retry_separately(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    // Left underived: the mover derives it when pushrebase prepares.
    let competitor = commit(&ctx, &repo, vec![root], vec![("c".into(), "c".into())]).await?;
    let mover = |attempt| {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook: Box<dyn PushrebaseHook> = Box::new(MoveOnAttempt {
            repo: repo.clone(),
            target: competitor,
            attempt,
            calls: calls.clone(),
        });
        (hook, calls)
    };

    // Direct: attempt 0 prepares the root's manifest, attempt 1 loses its
    // swap, attempt 2 lands.
    let (move_hook, calls) = mover(1);
    let (guard_hook, counters) =
        SourceRevisionPushrebaseHook::try_new_with_counters(&guard_config()?, &repo)?;
    let hooks = vec![move_hook, guard_hook];
    let pushed = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let outcome = land(&ctx, &repo, &hooks, pushed).await?;
    assert_eq!(outcome.retry_num.0, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(counters.counts().prepared, 1);
    assert_eq!(counters.counts().preparation_required, 1);
    assert_eq!(counters.counts().rejected, 0);
    assert_eq!(contention_retries(outcome.retry_num.0, 1), 1);

    // Batched: pass 0 prepares, pass 1 loses its swap, pass 2 lands.
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = init_master(&ctx, &repo, Some(&Generated::new(Some("5")))).await?;
    let competitor = commit(&ctx, &repo, vec![root], vec![("c".into(), "c".into())]).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let hooks: Vec<Box<dyn PushrebaseHook>> = vec![
        Box::new(MoveOnAttempt {
            repo: repo.clone(),
            target: competitor,
            attempt: 1,
            calls: calls.clone(),
        }),
        SourceRevisionPushrebaseHook::try_new(&guard_config()?, &repo)?,
    ];
    let flags = PushrebaseFlags::default();
    let pushed = generate(
        &ctx,
        &repo,
        root,
        &Generated::new(Some("6")).with(0, 0, "x"),
    )
    .await?;
    let (request, rx) = queued(&ctx, &repo, &[pushed]).await?;
    assert!(
        repo.repo_derived_data()
            .fetch_derived::<RootContentManifestId>(&ctx, root)
            .await?
            .is_none(),
        "the root needs preparation in pass 0"
    );
    let mut requests = vec![request];
    for (retry_num, preparation_rounds) in [(1, 1), (2, 1)] {
        requests =
            do_batched_pushrebase(&ctx, &repo, &flags, &master_bookmark(), requests, &hooks).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].retry_num.0, retry_num);
        assert_eq!(requests[0].preparation_rounds, preparation_rounds);
    }
    assert!(
        do_batched_pushrebase(&ctx, &repo, &flags, &master_bookmark(), requests, &hooks)
            .await
            .is_empty()
    );
    assert_eq!(rx.await??.retry_num.0, 2);
    assert_eq!(contention_retries(2, 1), 1);
    Ok(())
}

/// The generated files with tracked string entries: `entries` of key, text
/// and metadata, in one module, in both directories.
fn tracked_files(root: i64, entries: &[(&str, &str, Option<(i64, i64)>)]) -> Vec<(String, String)> {
    let strings: Vec<String> = entries
        .iter()
        .map(|(key, text, tracked)| {
            let metadata = tracked.map_or_else(String::new, |(introduced, last_changed)| {
                format!(
                    ",\n                    \"introduced_revision\": {introduced},\n                    \"last_changed_revision\": {last_changed}"
                )
            });
            format!(
                "                {{\n                    \"comment\": \"c\",\n                    \"en_string\": \"{text}\",\n                    \"key\": \"{key}\"{metadata}\n                }}"
            )
        })
        .collect();
    let json = format!(
        "{{\n    \"source_revision\": {root},\n    \"modules\": [\n        {{\n            \"key\": \"default\",\n            \"strings\": [\n{}\n            ]\n        }}\n    ]\n}}\n",
        strings.join(",\n")
    );
    [AUTHORITATIVE, MIRROR]
        .map(|dir| (format!("{dir}/default.json"), json.clone()))
        .to_vec()
}

/// Tracking through real pushrebases: the metadata-only migration of root 1
/// lands, then an edit with the right metadata; tampered metadata and
/// dropping it are rejected, and counted as rejections before the swap.
#[mononoke::fbinit_test]
async fn guard_tracks_string_revisions(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = commit(
        &ctx,
        &repo,
        vec![],
        tracked_files(1, &[("A", "a", None), ("B", "b", None)]),
    )
    .await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(root)
        .await?;
    let (hooks, counters) = counted_guard(&repo)?;

    // Combined with a source change, the migration is rejected.
    let combined = commit(
        &ctx,
        &repo,
        vec![root],
        tracked_files(
            2,
            &[("A", "edited", Some((1, 2))), ("B", "b", Some((1, 1)))],
        ),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, combined).await),
        TransitionError::MetadataMigrationChangesSource
    );

    let migration = commit(
        &ctx,
        &repo,
        vec![root],
        tracked_files(1, &[("A", "a", Some((1, 1))), ("B", "b", Some((1, 1)))]),
    )
    .await?;
    land(&ctx, &repo, &hooks, migration).await?;

    let head = master(&ctx, &repo).await?;
    let edit = commit(
        &ctx,
        &repo,
        vec![head],
        tracked_files(
            2,
            &[
                ("A", "edited", Some((1, 2))),
                ("B", "b", Some((1, 1))),
                ("C", "c", Some((2, 2))),
            ],
        ),
    )
    .await?;
    land(&ctx, &repo, &hooks, edit).await?;

    let head = master(&ctx, &repo).await?;
    let before = counters.counts();
    let tampered = commit(
        &ctx,
        &repo,
        vec![head],
        tracked_files(
            2,
            &[
                ("A", "edited", Some((1, 2))),
                ("B", "b", Some((1, 2))),
                ("C", "c", Some((2, 2))),
            ],
        ),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, tampered).await),
        TransitionError::MetadataMismatch {
            key: "B".to_owned(),
            change: EntryChange::Unchanged,
            expected: EntryRevisions {
                introduced: revision(1),
                last_changed: revision(1),
            },
            found: EntryRevisions {
                introduced: revision(1),
                last_changed: revision(2),
            },
        }
    );
    let after = counters.counts();
    assert_eq!(after.rejected, before.rejected + 1);
    assert_eq!(after.rejected_layout, before.rejected_layout);

    let dropped = commit(
        &ctx,
        &repo,
        vec![head],
        tracked_files(
            3,
            &[("A", "again", None), ("B", "b", None), ("C", "c", None)],
        ),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, dropped).await),
        TransitionError::MetadataRemoved
    );
    assert_eq!(master(&ctx, &repo).await?, head);
    Ok(())
}

/// Malformed catalogs through real pushrebases: a tracked landing cannot
/// hide a duplicate or untracked string in a part of the catalog that is not
/// the generated shape, in either directory, and cannot drop every tracked
/// entry. The bookmark stays put.
#[mononoke::fbinit_test]
async fn guard_rejects_malformed_tracked_catalogs(fb: FacebookInit) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let root = commit(
        &ctx,
        &repo,
        vec![],
        tracked_files(1, &[("A", "a", Some((1, 1))), ("B", "b", Some((1, 1)))]),
    )
    .await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(root)
        .await?;
    let hooks = guard(&repo)?;
    let tracked = tracked_files(
        2,
        &[("A", "a", Some((1, 1))), ("B", "edited", Some((1, 2)))],
    );
    let hidden_module = |strings: &str| {
        format!(
            r#"{{"source_revision": 2, "modules": [{{"key": "m", "strings": [{{"comment": "c", "en_string": "a", "key": "A", "introduced_revision": 1, "last_changed_revision": 1}}]}}, {{"key": "n", "strings": {strings}}}]}}"#
        )
    };
    let duplicate = r#"{"0": {"comment": "c", "en_string": "other", "key": "A"}}"#;

    for (files, what) in [
        // A second file whose module hides a duplicate of `A`, both mirrors.
        (
            tracked
                .iter()
                .cloned()
                .chain(
                    [AUTHORITATIVE, MIRROR]
                        .map(|dir| (format!("{dir}/hidden.json"), hidden_module(duplicate))),
                )
                .collect::<Vec<_>>(),
            "hidden duplicate",
        ),
        // `strings` null in the only file.
        (
            [AUTHORITATIVE, MIRROR]
                .map(|dir| (format!("{dir}/default.json"), hidden_module("null")))
                .to_vec(),
            "null strings",
        ),
        // A module that is not an object.
        (
            [AUTHORITATIVE, MIRROR]
                .map(|dir| {
                    (
                        format!("{dir}/default.json"),
                        hidden_module("[]").replace(r#"{"key": "n", "strings": []}"#, r#""n""#),
                    )
                })
                .to_vec(),
            "module string",
        ),
    ] {
        let pushed = commit(&ctx, &repo, vec![root], files).await?;
        let error = guard_error(land(&ctx, &repo, &hooks, pushed).await);
        assert!(
            matches!(
                &error,
                SourceRevisionGuardError::InvalidCandidate {
                    error: SnapshotError::MalformedCatalog { .. },
                    ..
                }
            ),
            "{what}: {error}"
        );
    }

    // Every tracked entry dropped, the shape kept, the root bumped.
    let empty = [AUTHORITATIVE, MIRROR]
        .map(|dir| {
            (
                format!("{dir}/default.json"),
                r#"{"source_revision": 2, "modules": [{"key": "default", "strings": []}]}"#
                    .to_string(),
            )
        })
        .to_vec();
    let emptied = commit(&ctx, &repo, vec![root], empty).await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, emptied).await),
        TransitionError::MetadataRemoved
    );
    assert_eq!(master(&ctx, &repo).await?, root);

    // A well-formed tracked landing still goes through.
    let fine = commit(&ctx, &repo, vec![root], tracked).await?;
    land(&ctx, &repo, &hooks, fine).await?;
    Ok(())
}

/// A string hidden in a malformed catalog of an untracked root-1 snapshot
/// cannot be exposed by the tracking migration: that changes the source.
#[mononoke::fbinit_test]
async fn guard_does_not_date_strings_exposed_from_a_malformed_catalog(
    fb: FacebookInit,
) -> Result<()> {
    init_just_knobs_for_guard_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let hidden = r#"{"source_revision": 1, "modules": [{"key": "m", "strings": {"0": {"comment": "c", "en_string": "a", "key": "A"}}}]}"#;
    let root = commit(
        &ctx,
        &repo,
        vec![],
        [AUTHORITATIVE, MIRROR]
            .map(|dir| (format!("{dir}/default.json"), hidden.to_string()))
            .to_vec(),
    )
    .await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(root)
        .await?;
    let hooks = guard(&repo)?;

    let exposed = commit(
        &ctx,
        &repo,
        vec![root],
        tracked_files(1, &[("A", "a", Some((1, 1)))]),
    )
    .await?;
    assert_eq!(
        transition_error(land(&ctx, &repo, &hooks, exposed).await),
        TransitionError::MetadataMigrationChangesSource
    );
    assert_eq!(master(&ctx, &repo).await?, root);
    Ok(())
}
