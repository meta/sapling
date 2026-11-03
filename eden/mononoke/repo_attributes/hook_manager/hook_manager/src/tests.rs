/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Error;
use async_trait::async_trait;
use bookmarks_types::BookmarkKey;
use context::CoreContext;
use fbinit::FacebookInit;
use maplit::hashmap;
use maplit::hashset;
use metaconfig_types::ComparableRegex;
use metaconfig_types::HookBypassActor;
use metaconfig_types::HookBypassCondition;
use metaconfig_types::HookConfig;
use metaconfig_types::HookManagerParams;
use mononoke_macros::mononoke;
use mononoke_types::BasicFileChange;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ContentId;
use mononoke_types::DateTime;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use mononoke_types::NonRootMPath;
use mononoke_types_mocks::contentid::FIVES_CTID;
use mononoke_types_mocks::contentid::FOURS_CTID;
use mononoke_types_mocks::contentid::ONES_CTID;
use mononoke_types_mocks::contentid::SIXES_CTID;
use mononoke_types_mocks::contentid::THREES_CTID;
use mononoke_types_mocks::contentid::TWOS_CTID;
use permission_checker::AlwaysMember;
use permission_checker::ArcMembershipChecker;
use permission_checker::InternalAclProvider;
use permission_checker::MemberAllowlist;
use permission_checker::MembershipChecker;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use permission_checker::NeverMember;
use repo_permission_checker::NeverAllowRepoPermissionChecker;
use scuba::ScubaValue;
use scuba_ext::MononokeScubaSampleBuilder;
use sorted_vector_map::sorted_vector_map;

use crate::BypassCheckers;
use crate::BypassDecision;
use crate::BypassIdentitySource;
use crate::ChangesetHook;
use crate::CheckedBypassIdentities;
use crate::CrossRepoPushSource;
use crate::FileHook;
use crate::HookExecution;
use crate::HookExecutionPurpose;
use crate::HookManager;
use crate::HookOutcome;
use crate::HookRejectionInfo;
use crate::HookRepo;
use crate::LogOnlyRejections;
use crate::PushAuthoredBy;
use crate::Pushvars;
use crate::add_log_only_bypass_columns;

#[derive(Clone, Debug)]
struct FnChangesetHook {
    f: fn() -> HookExecution,
}

impl FnChangesetHook {
    fn new(f: fn() -> HookExecution) -> FnChangesetHook {
        FnChangesetHook { f }
    }
}

#[async_trait]
impl ChangesetHook for FnChangesetHook {
    async fn run<'this: 'cs, 'ctx: 'this, 'cs, 'repo: 'cs>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        _bookmark: &BookmarkKey,
        _changeset: &'cs BonsaiChangeset,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
        _maybe_pushvars: Option<&'cs Pushvars>,
    ) -> Result<HookExecution, Error> {
        Ok((self.f)())
    }
}

fn always_accepting_changeset_hook() -> Box<dyn ChangesetHook> {
    let f: fn() -> HookExecution = || HookExecution::accepted();
    Box::new(FnChangesetHook::new(f))
}

fn always_rejecting_changeset_hook() -> Box<dyn ChangesetHook> {
    let f: fn() -> HookExecution = default_rejection;
    Box::new(FnChangesetHook::new(f))
}

struct DryRunAwareChangesetHook;

#[async_trait]
impl ChangesetHook for DryRunAwareChangesetHook {
    async fn run<'this: 'cs, 'ctx: 'this, 'cs, 'repo: 'cs>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        _bookmark: &BookmarkKey,
        _changeset: &'cs BonsaiChangeset,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
        _maybe_pushvars: Option<&'cs Pushvars>,
    ) -> Result<HookExecution, Error> {
        Ok(default_rejection())
    }

    async fn run_with_purpose<'this: 'cs, 'ctx: 'this, 'cs, 'repo: 'cs>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        _bookmark: &BookmarkKey,
        _changeset: &'cs BonsaiChangeset,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
        purpose: HookExecutionPurpose,
        _maybe_pushvars: Option<&'cs Pushvars>,
    ) -> Result<HookExecution, Error> {
        assert_eq!(purpose, HookExecutionPurpose::DryRun);
        Ok(HookExecution::accepted())
    }
}

#[derive(Clone, Debug)]
struct ContentIdMatchingChangesetHook {
    expected_content_ids: HashMap<NonRootMPath, Option<ContentId>>,
}

#[async_trait]
impl ChangesetHook for ContentIdMatchingChangesetHook {
    async fn run<'this: 'cs, 'ctx: 'this, 'cs, 'repo: 'cs>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        _bookmark: &BookmarkKey,
        changeset: &'cs BonsaiChangeset,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
        _maybe_pushvars: Option<&'cs Pushvars>,
    ) -> Result<HookExecution, Error> {
        for (path, change) in changeset.simplified_file_changes() {
            // If we have a change to a path, but no expected change, fail
            let expected_content = match self.expected_content_ids.get(path) {
                Some(expected) => expected,
                None => return Ok(default_rejection()),
            };

            if change.map(|change| change.content_id()) != *expected_content {
                return Ok(default_rejection());
            }
        }

        Ok(HookExecution::accepted())
    }
}

fn content_id_matching_changeset_hook(
    expected_content_ids: HashMap<NonRootMPath, Option<ContentId>>,
) -> Box<dyn ChangesetHook> {
    Box::new(ContentIdMatchingChangesetHook {
        expected_content_ids,
    })
}

#[derive(Clone, Debug)]
struct FnFileHook {
    f: fn() -> HookExecution,
}

impl FnFileHook {
    fn new(f: fn() -> HookExecution) -> FnFileHook {
        FnFileHook { f }
    }
}

#[async_trait]
impl FileHook for FnFileHook {
    async fn run<'this: 'change, 'ctx: 'this, 'change, 'repo: 'change, 'path: 'change>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        _change: Option<&'change BasicFileChange>,
        _path: &'path NonRootMPath,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
    ) -> Result<HookExecution, Error> {
        Ok((self.f)())
    }
}

fn always_accepting_file_hook() -> Box<dyn FileHook> {
    let f: fn() -> HookExecution = || HookExecution::accepted();
    Box::new(FnFileHook::new(f))
}

fn always_rejecting_file_hook() -> Box<dyn FileHook> {
    let f: fn() -> HookExecution = default_rejection;
    Box::new(FnFileHook::new(f))
}

#[derive(Clone, Debug)]
struct PathMatchingFileHook {
    paths: HashSet<NonRootMPath>,
}

#[async_trait]
impl FileHook for PathMatchingFileHook {
    async fn run<'this: 'change, 'ctx: 'this, 'change, 'repo: 'change, 'path: 'change>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        _change: Option<&'change BasicFileChange>,
        path: &'path NonRootMPath,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
    ) -> Result<HookExecution, Error> {
        Ok(if self.paths.contains(path) {
            HookExecution::accepted()
        } else {
            default_rejection()
        })
    }
}

fn path_matching_file_hook(paths: HashSet<NonRootMPath>) -> Box<dyn FileHook> {
    Box::new(PathMatchingFileHook { paths })
}

#[derive(Clone, Debug)]
struct ContentIdMatchingFileHook {
    expected_content_id: Option<ContentId>,
}

#[async_trait]
impl FileHook for ContentIdMatchingFileHook {
    async fn run<'this: 'change, 'ctx: 'this, 'change, 'repo: 'change, 'path: 'change>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        change: Option<&'change BasicFileChange>,
        _path: &'path NonRootMPath,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
    ) -> Result<HookExecution, Error> {
        if change.map(|change| change.content_id()) == self.expected_content_id {
            Ok(HookExecution::accepted())
        } else {
            Ok(default_rejection())
        }
    }
}

fn content_id_matching_file_hook(expected_content_id: Option<ContentId>) -> Box<dyn FileHook> {
    Box::new(ContentIdMatchingFileHook {
        expected_content_id,
    })
}

#[derive(Clone, Debug)]
struct IsSymLinkMatchingFileHook {
    is_symlink: bool,
}

#[async_trait]
impl FileHook for IsSymLinkMatchingFileHook {
    async fn run<'this: 'change, 'ctx: 'this, 'change, 'repo: 'change, 'path: 'change>(
        &'this self,
        _ctx: &'ctx CoreContext,
        _repo: &'repo HookRepo,
        change: Option<&'change BasicFileChange>,
        _path: &'path NonRootMPath,
        _cross_repo_push_source: CrossRepoPushSource,
        _push_authored_by: PushAuthoredBy,
    ) -> Result<HookExecution, Error> {
        let is_symlink = match change {
            Some(change) => change.file_type() == FileType::Symlink,
            None => false,
        };
        Ok(if self.is_symlink == is_symlink {
            HookExecution::accepted()
        } else {
            default_rejection()
        })
    }
}

fn is_symlink_matching_file_hook(is_symlink: bool) -> Box<dyn FileHook> {
    Box::new(IsSymLinkMatchingFileHook { is_symlink })
}

async fn setup_hook_manager(
    fb: FacebookInit,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
) -> HookManager {
    let ctx = CoreContext::test_mock(fb);
    let repo = test_repo_factory::build_empty(ctx.fb)
        .await
        .expect("Failed to construct repo");

    let mut hook_manager = HookManager::new(
        ctx.fb,
        &InternalAclProvider::default(),
        repo,
        HookManagerParams {
            disable_acl_checker: true,
            ..Default::default()
        },
        Arc::new(NeverAllowRepoPermissionChecker {}),
        MononokeScubaSampleBuilder::with_discard(),
        "zoo".to_string(),
    )
    .await
    .expect("Failed to construct HookManager");

    for (bookmark_name, hook_names) in bookmarks {
        hook_manager
            .set_hooks_for_bookmark(BookmarkKey::new(bookmark_name).unwrap().into(), hook_names);
    }
    for (regx, hook_names) in regexes {
        hook_manager
            .set_hooks_for_bookmark(ComparableRegex::new(&regx).unwrap().into(), hook_names);
    }
    hook_manager
}

fn default_rejection() -> HookExecution {
    HookExecution::rejected(HookRejectionInfo::new_long("desc", "long_desc".to_string()))
}

fn to_mpath(string: &str) -> NonRootMPath {
    NonRootMPath::new(string).unwrap()
}

fn default_changeset() -> BonsaiChangeset {
    BonsaiChangesetMut {
        author: "Jeremy Fitzhardinge <jsgf@fb.com>".to_string(),
        author_date: DateTime::from_timestamp(1584887580, 0).expect("Getting timestamp"),
        message: "This is a commit message".to_string(),
        file_changes: sorted_vector_map!{
            to_mpath("dir1/subdir1/subsubdir1/file_1") => FileChange::tracked(ONES_CTID, FileType::Symlink, 15, None, GitLfs::FullContent),
            to_mpath("dir1/subdir1/subsubdir2/file_1") => FileChange::tracked(TWOS_CTID, FileType::Regular, 17, None, GitLfs::FullContent),
            to_mpath("dir1/subdir1/subsubdir2/file_2") => FileChange::tracked(THREES_CTID, FileType::Regular, 2, None, GitLfs::FullContent),
        },
        ..Default::default()
    }.freeze().expect("Created changeset")
}

async fn run_changeset_hooks(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn ChangesetHook>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HookExecution>,
) {
    let mut hook_manager = setup_hook_manager(ctx.fb, bookmarks, regexes).await;
    for (hook_name, hook) in hooks {
        hook_manager.register_changeset_hook(
            &hook_name,
            hook,
            Default::default(),
            BypassCheckers::new(),
        );
    }

    let changeset = default_changeset();
    let res = hook_manager
        .run_changesets_hooks_for_bookmark(
            &ctx,
            &[changeset],
            &BookmarkKey::new(bookmark_name).unwrap(),
            None,
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::LandAttempt,
            None,
            LogOnlyRejections::Suppress,
        )
        .await
        .unwrap();
    let map: HashMap<String, HookExecution> = res
        .into_iter()
        .map(|outcome| (outcome.get_hook_name().to_string(), outcome.into()))
        .collect();
    assert_eq!(expected, map);
}

async fn run_file_hooks(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn FileHook>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HashMap<String, HookExecution>>,
) {
    let cs = default_changeset();
    let mut hook_manager = setup_hook_manager(ctx.fb, bookmarks, regexes).await;
    for (hook_name, hook) in hooks {
        hook_manager.register_file_hook(
            &hook_name,
            hook,
            Default::default(),
            BypassCheckers::new(),
        );
    }
    let res = hook_manager
        .run_changesets_hooks_for_bookmark(
            &ctx,
            &[cs],
            &BookmarkKey::new(bookmark_name).unwrap(),
            None,
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::LandAttempt,
            None,
            LogOnlyRejections::Suppress,
        )
        .await
        .unwrap();
    let map: HashMap<String, HashMap<String, HookExecution>> =
        res.into_iter().fold(HashMap::new(), |mut m, outcome| {
            let path = outcome.get_file_path().expect("Changeset hook").to_string();
            match m.entry(outcome.get_hook_name().to_string()) {
                Entry::Vacant(v) => v.insert(HashMap::new()).insert(path, outcome.into()),
                Entry::Occupied(mut v) => v.get_mut().insert(path, outcome.into()),
            };
            m
        });
    assert_eq!(expected, map);
}

#[mononoke::fbinit_test]
async fn test_changeset_hook_accepted(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn ChangesetHook>> = hashmap! {
        "hook1".to_string() => always_accepting_changeset_hook()
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {};
    let expected = hashmap! {
        "hook1".to_string() => HookExecution::accepted()
    };
    run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_changeset_hook_receives_execution_purpose(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
    hook_manager.register_changeset_hook(
        "hook1",
        Box::new(DryRunAwareChangesetHook),
        Default::default(),
        BypassCheckers::new(),
    );
    let bookmark = BookmarkKey::new("bm1").unwrap();
    hook_manager.set_hooks_for_bookmark(bookmark.clone().into(), vec!["hook1".to_string()]);

    let outcomes = hook_manager
        .run_changesets_hooks_for_bookmark(
            &ctx,
            &[default_changeset()],
            &bookmark,
            None,
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::DryRun,
            None,
            LogOnlyRejections::Suppress,
        )
        .await
        .unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].get_execution(), &HookExecution::accepted());
}

#[mononoke::fbinit_test]
async fn test_changeset_hook_rejected(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn ChangesetHook>> = hashmap! {
        "hook1".to_string() => always_rejecting_changeset_hook()
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {};
    let expected = hashmap! {
        "hook1".to_string() => default_rejection()
    };
    run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_log_only_rejection_reported_per_mode(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let mut hook_manager = setup_hook_manager(
        ctx.fb,
        hashmap! {"bm1".to_string() => vec!["hook1".to_string()]},
        hashmap! {},
    )
    .await;
    hook_manager.register_changeset_hook(
        "hook1",
        always_rejecting_changeset_hook(),
        HookConfig {
            log_only: true,
            ..Default::default()
        },
        BypassCheckers::new(),
    );
    let changeset = default_changeset();
    let bookmark = BookmarkKey::new("bm1").unwrap();

    let outcomes = hook_manager
        .run_changesets_hooks_for_bookmark(
            &ctx,
            std::slice::from_ref(&changeset),
            &bookmark,
            None,
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::LandAttempt,
            None,
            LogOnlyRejections::Suppress,
        )
        .await
        .unwrap();
    assert!(
        !outcomes[0].get_execution().is_rejected(),
        "a log-only rejection must be reported as accepted on a real push"
    );

    let outcomes = hook_manager
        .run_changesets_hooks_for_bookmark(
            &ctx,
            std::slice::from_ref(&changeset),
            &bookmark,
            None,
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::LandAttempt,
            None,
            LogOnlyRejections::Report,
        )
        .await
        .unwrap();
    assert!(
        outcomes[0].get_execution().is_rejected(),
        "LogOnlyRejections::Report must surface the underlying log-only rejection"
    );
}

#[mononoke::fbinit_test]
async fn test_changeset_hook_mix(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn ChangesetHook>> = hashmap! {
        "hook1".to_string() => always_accepting_changeset_hook(),
        "hook2".to_string() => always_rejecting_changeset_hook(),
        "hook3".to_string() => always_accepting_changeset_hook(),
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()]
    };
    let regexes = hashmap! {
        "b.*".to_string() => vec!["hook3".to_string()],
    };
    let expected = hashmap! {
        "hook1".to_string() => HookExecution::accepted(),
        "hook2".to_string() => default_rejection(),
        "hook3".to_string() => HookExecution::accepted(),
    };
    run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_changeset_hook_content_id(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hook1_map = hashmap! {
        to_mpath("dir1/subdir1/subsubdir1/file_1") => Some(ONES_CTID),
        to_mpath("dir1/subdir1/subsubdir2/file_1") => Some(TWOS_CTID),
        to_mpath("dir1/subdir1/subsubdir2/file_2") => Some(THREES_CTID),
    };
    let hook2_map = hashmap! {
        to_mpath("dir1/subdir1/subsubdir1/file_1") => Some(FOURS_CTID),
        to_mpath("dir1/subdir1/subsubdir2/file_1") => Some(TWOS_CTID),
        to_mpath("dir1/subdir1/subsubdir2/file_2") => Some(THREES_CTID),
    };
    let hook3_map = hashmap! {
        to_mpath("dir1/subdir1/subsubdir1/file_1") => Some(FOURS_CTID),
        to_mpath("dir1/subdir1/subsubdir2/file_1") => Some(FIVES_CTID),
        to_mpath("dir1/subdir1/subsubdir2/file_2") => Some(SIXES_CTID),
    };
    let hooks: HashMap<String, Box<dyn ChangesetHook>> = hashmap! {
        "hook1".to_string() => content_id_matching_changeset_hook(hook1_map),
        "hook2".to_string() => content_id_matching_changeset_hook(hook2_map),
        "hook3".to_string() => content_id_matching_changeset_hook(hook3_map),
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()]
    };
    let regexes = hashmap! {
        "b.*".to_string() => vec!["hook2".to_string(), "hook3".to_string()]
    };
    let expected = hashmap! {
        "hook1".to_string() => HookExecution::accepted(),
        "hook2".to_string() => default_rejection(),
        "hook3".to_string() => default_rejection(),
    };
    run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hook_accepted(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => always_accepting_file_hook()
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {};
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::accepted(),
        }
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hook_rejected(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => always_rejecting_file_hook()
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {};
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
        }
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hook_mix(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => always_rejecting_file_hook(),
        "hook2".to_string() => always_accepting_file_hook()
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {
        "b.*".to_string() => vec!["hook2".to_string()]
    };
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
        },
        "hook2".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::accepted(),
        }
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hooks_paths(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let matching_paths = hashset![
        to_mpath("dir1/subdir1/subsubdir2/file_1"),
        to_mpath("dir1/subdir1/subsubdir2/file_2"),
    ];
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => path_matching_file_hook(matching_paths),
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {};
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::accepted(),
        }
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hooks_paths_mix(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let matching_paths1 = hashset![
        to_mpath("dir1/subdir1/subsubdir2/file_1"),
        to_mpath("dir1/subdir1/subsubdir2/file_2"),
    ];
    let matching_paths2 = hashset![to_mpath("dir1/subdir1/subsubdir1/file_1"),];
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => path_matching_file_hook(matching_paths1),
        "hook2".to_string() => path_matching_file_hook(matching_paths2),
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()]
    };
    let regexes = hashmap! {
        "b.*".to_string() => vec!["hook2".to_string()]
    };
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::accepted(),
        },
        "hook2".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
        }
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hook_content_id(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => content_id_matching_file_hook(Some(ONES_CTID)),
        "hook2".to_string() => content_id_matching_file_hook(Some(TWOS_CTID)),
        "hook3".to_string() => content_id_matching_file_hook(Some(THREES_CTID)),
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()],
    };
    let regexes = hashmap! {
        "b.*".to_string() => vec!["hook3".to_string()],
    };
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
        },
        "hook2".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
        },
        "hook3".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::accepted(),
        },
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

#[mononoke::fbinit_test]
async fn test_file_hook_is_symlink(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let hooks: HashMap<String, Box<dyn FileHook>> = hashmap! {
        "hook1".to_string() => is_symlink_matching_file_hook(true),
        "hook2".to_string() => is_symlink_matching_file_hook(false),
    };
    let bookmarks = hashmap! {
        "bm1".to_string() => vec!["hook1".to_string()],
    };
    let regexes = hashmap! {
        "b.*".to_string() => vec!["hook2".to_string()],
    };
    let expected = hashmap! {
        "hook1".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
        },
        "hook2".to_string() => hashmap! {
            "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
            "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::accepted(),
            "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::accepted(),
        },
    };
    run_file_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
}

// =========================================================================
// Bypass permission group tests
// =========================================================================

fn bypass_permission_groups_jk(
    use_client_identities: bool,
) -> justknobs::test_helpers::JustKnobsInMemory {
    justknobs::test_helpers::JustKnobsInMemory::new(
        [
            (
                "scm/mononoke:check_hook_bypass_permission_group_with_client_identities"
                    .to_string(),
                justknobs::test_helpers::KnobVal::Bool(use_client_identities),
            ),
            (
                "scm/mononoke:resolve_unixname_from_employee_service_for_hook_bypass".to_string(),
                justknobs::test_helpers::KnobVal::Bool(true),
            ),
        ]
        .into(),
    )
}

/// Build a `CoreContext` carrying the given client identities.
fn ctx_with_identities(fb: FacebookInit, identities: MononokeIdentitySet) -> CoreContext {
    let metadata = metadata::Metadata::default().set_identities(identities);
    let session = context::SessionContainer::builder(fb)
        .metadata(Arc::new(metadata))
        .build();
    CoreContext::test_mock_session(session)
}

/// Plain `USER:<unixname>` client identities, as a human pushing with a regular
/// certificate presents. These carry no credential attributes, so
/// `likely_an_agent()` is false.
fn user_identities(unixnames: &[&str]) -> MononokeIdentitySet {
    unixnames
        .iter()
        .map(|name| MononokeIdentity::from_legacy_type_data("USER", *name))
        .collect()
}

/// Client identities carrying the `agent` attribute that an agent's certificate
/// presents, so `likely_an_agent()` is true.
///
/// fbcode-only: the OSS `MononokeIdentitySetExt::likely_an_agent` is a constant
/// `false` stub and `try_from_json_encoded` is unimplemented there, so there is
/// no way to build an agent identity in an OSS build.
#[cfg(fbcode_build)]
fn agent_identities(unixname: &str) -> MononokeIdentitySet {
    MononokeIdentity::try_from_json_encoded(&format!(
        r#"{{"authn": ["mid://TEST/USER/{unixname}?agent.id=AGENT%3aclaude_code"]}}"#
    ))
    .expect("valid authn identity envelope")
}

fn changeset_with_bypass_msg() -> BonsaiChangeset {
    BonsaiChangesetMut {
        author: "Test User <test@fb.com>".to_string(),
        author_date: DateTime::from_timestamp(1584887580, 0).expect("Getting timestamp"),
        message: "This commit has @bypass_hook in the message".to_string(),
        file_changes: sorted_vector_map! {
            to_mpath("dir1/file.txt") => FileChange::tracked(ONES_CTID, FileType::Regular, 10, None, GitLfs::FullContent),
        },
        ..Default::default()
    }
    .freeze()
    .expect("Created changeset")
}

const BYPASS_GROUP: &str = "test_bypass_group";

/// Membership checkers for a hook whose bypass conditions use `BYPASS_GROUP`.
fn bypass_checkers(checker: ArcMembershipChecker) -> BypassCheckers {
    BypassCheckers::from([(BYPASS_GROUP.to_string(), checker)])
}

fn bypass_config_no_group() -> HookConfig {
    HookConfig {
        bypasses: vec![
            HookBypassCondition::commit_message("@bypass_hook").with_actor(HookBypassActor::Any),
        ],
        ..Default::default()
    }
}

fn bypass_config_with_group() -> HookConfig {
    HookConfig {
        bypasses: vec![
            HookBypassCondition::commit_message("@bypass_hook").with_permission_group(BYPASS_GROUP),
        ],
        ..Default::default()
    }
}

/// A group-gated commit-message bypass that agents in the group may also use.
#[cfg(fbcode_build)]
fn bypass_config_with_group_allowing_agents() -> HookConfig {
    HookConfig {
        bypasses: vec![
            HookBypassCondition::commit_message("@bypass_hook")
                .with_permission_group(BYPASS_GROUP)
                .with_actor(HookBypassActor::Any),
        ],
        ..Default::default()
    }
}

/// A group-gated bypass with no marker: membership alone bypasses the hook.
fn implicit_bypass_config_with_group() -> HookConfig {
    HookConfig {
        bypasses: vec![HookBypassCondition::permission_group(BYPASS_GROUP)],
        ..Default::default()
    }
}

fn pushvar_bypass_config_with_group() -> HookConfig {
    HookConfig {
        bypasses: vec![
            HookBypassCondition::pushvar("BYPASS", "true").with_permission_group(BYPASS_GROUP),
        ],
        ..Default::default()
    }
}

/// With no permission group configured, a bypass string still bypasses the hook.
#[mononoke::fbinit_test]
async fn test_bypass_no_group_preserves_behavior(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: bypass_config_no_group(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_skipped_pre_run(&res);
}

/// With no permission group and no bypass string, the hook runs normally.
#[mononoke::fbinit_test]
async fn test_bypass_no_group_no_string_runs_hook(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: bypass_config_no_group(),
        changeset: default_changeset(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

/// Group configured, bypass string present, user in group: bypassed.
#[mononoke::fbinit_test]
async fn test_bypass_with_group_authorized_user(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(AlwaysMember::new().into()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// Group configured, bypass string present, user not in group: the bypass is
/// ignored and the hook runs.
#[mononoke::fbinit_test]
async fn test_bypass_with_group_unauthorized_user(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(NeverMember::new().into()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

/// Group configured but no bypass string: the hook runs.
#[mononoke::fbinit_test]
async fn test_bypass_with_group_no_bypass_string(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(NeverMember::new().into()),
        changeset: default_changeset(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

/// Group configured, pushvar bypass, user in group: bypassed.
#[mononoke::fbinit_test]
async fn test_pushvar_bypass_with_group_authorized(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: pushvar_bypass_config_with_group(),
        checker: Some(AlwaysMember::new().into()),
        changeset: default_changeset(),
        pushvars: Some(bypass_pushvars()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// Group configured, pushvar bypass, user not in group: the bypass is ignored
/// and the hook runs.
#[mononoke::fbinit_test]
async fn test_pushvar_bypass_with_group_unauthorized(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: pushvar_bypass_config_with_group(),
        checker: Some(NeverMember::new().into()),
        changeset: default_changeset(),
        pushvars: Some(bypass_pushvars()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

/// An unauthorized bypass on a commit the hook would accept is moot: the hook
/// runs and the push succeeds.
#[mononoke::fbinit_test]
async fn test_unauthorized_bypass_with_accepting_hook(fb: FacebookInit) {
    let res = BypassScenario {
        hook: always_accepting_changeset_hook(),
        checker: Some(NeverMember::new().into()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_accepted(&res);
}

/// A present bypass token makes the permission group consulted even for an
/// accepting hook (the decision is eager), but the accept still wins. The
/// membership checker is consulted twice for the one (hook, changeset): once for
/// the client identity, once for the author fallback.
#[mononoke::fbinit_test]
async fn test_accepting_hook_with_bypass_checks_group_once(fb: FacebookInit) {
    let (checker, calls) = CountingMember::new(false);
    let res = BypassScenario {
        hook: always_accepting_changeset_hook(),
        checker: Some(checker),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_accepted(&res);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the eager bypass decision consults the permission group once per (hook, \
         changeset) for the client identity plus once for the author fallback (2 total)",
    );
}

/// A file hook rejecting several paths of one changeset under an unauthorized
/// bypass checks the permission group once per (hook, changeset), not per path,
/// while emitting one annotated rejection per rejected path.
#[mononoke::fbinit_test]
async fn test_unauthorized_bypass_file_hook_annotates_each_path(fb: FacebookInit) {
    let ctx = ctx_with_identities(fb, MononokeIdentitySet::new());
    let (checker, calls) = CountingMember::new(false);
    let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
    hook_manager.register_file_hook(
        "hook1",
        always_rejecting_file_hook(),
        pushvar_bypass_config_with_group(),
        bypass_checkers(checker),
    );
    let bm = BookmarkKey::new("bm1").unwrap();
    hook_manager.set_hooks_for_bookmark(bm.clone().into(), vec!["hook1".to_string()]);
    // default_changeset touches 3 files; the file hook rejects each one.
    let changesets = [default_changeset()];
    let res = justknobs::test_helpers::with_just_knobs_async(
        bypass_permission_groups_jk(true),
        Box::pin(hook_manager.run_changesets_hooks_for_bookmark(
            &ctx,
            &changesets,
            &bm,
            Some(&bypass_pushvars()),
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::LandAttempt,
            None,
            LogOnlyRejections::Suppress,
        )),
    )
    .await
    .unwrap();
    assert_eq!(
        res.len(),
        3,
        "expected one annotated rejection per rejected path, got {res:?}",
    );
    assert!(res.iter().all(|o| o.get_execution().is_rejected()));
    assert!(res.iter().all(|o| {
        o.get_execution()
            .rejection_info()
            .is_some_and(|info| info.long_description.contains("not a member of group"))
    }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "membership must be checked once per (hook, changeset) for the client identity \
         plus once for the author fallback (2 total), not once per rejected path (which \
         would be 6)",
    );
}

/// An unauthorized bypass on a commit the hook rejects: the hook's rejection
/// stands, annotated with a note that the bypass was ignored for lack of group
/// membership.
#[mononoke::fbinit_test]
async fn test_unauthorized_bypass_with_rejecting_hook_is_annotated(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(NeverMember::new().into()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_eq!(res.len(), 1, "expected one outcome, got {res:?}");
    let info = res[0]
        .get_execution()
        .rejection_info()
        .expect("hook rejected");
    // The hook's own reason is preserved...
    assert!(
        info.long_description.contains("long_desc"),
        "expected the hook's own reason, got {:?}",
        info.long_description,
    );
    // ...and the unauthorized-bypass note is appended.
    assert!(
        info.long_description.contains("not a member of group"),
        "expected the bypass note, got {:?}",
        info.long_description,
    );
}

/// When no bypass is attempted, a plain hook rejection is not annotated with the
/// bypass note.
#[mononoke::fbinit_test]
async fn test_no_bypass_rejection_is_not_annotated(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(NeverMember::new().into()),
        changeset: default_changeset(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_eq!(res.len(), 1, "expected one outcome, got {res:?}");
    let info = res[0]
        .get_execution()
        .rejection_info()
        .expect("hook rejected");
    assert!(
        !info.long_description.contains("bypass was ignored"),
        "did not expect a bypass note, got {:?}",
        info.long_description,
    );
}

// =========================================================================
// Author-based bypass permission group tests (use_client_identities off)
//
// These verify that group membership is checked against the changeset author's
// identity (USER:<unixname>), not the pusher's TLS cert identity.
// =========================================================================

/// On the changeset-author path, an author in the allowlist is bypassed.
#[mononoke::fbinit_test]
async fn test_bypass_checks_commit_author_not_pusher(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(allowlist(&["test"])),
        jk_use_client_identities: false,
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// On the changeset-author path, an author not in the allowlist is denied and
/// the hook runs.
#[mononoke::fbinit_test]
async fn test_bypass_rejects_when_author_not_in_allowlist(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(allowlist(&["someoneelse"])),
        jk_use_client_identities: false,
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

// =========================================================================
// Client-identity-based bypass permission group tests (use_client_identities on)
//
// These verify that, on the client-identities path, group membership is checked
// against the pusher's client identities first, falling back to the changeset
// author when the client identities are not in the group.
// =========================================================================

/// The client identity is in the allowlist while the author is not, so the
/// bypass is granted via the client identity.
#[mononoke::fbinit_test]
async fn test_bypass_with_client_identities_authorized(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(allowlist(&["client_user"])),
        client_identities: user_identities(&["client_user"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// The client identity is not in the allowlist but the author's unixname is, so
/// the client-identities path falls back to the author and honors the bypass.
#[mononoke::fbinit_test]
async fn test_bypass_with_client_identities_falls_back_to_author(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(allowlist(&["test"])),
        client_identities: user_identities(&["client_user"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// Neither the client identity nor the author is in the allowlist, so the
/// fallback to the author also fails and the hook runs.
#[mononoke::fbinit_test]
async fn test_bypass_with_client_identities_and_author_both_unauthorized_fails_closed(
    fb: FacebookInit,
) {
    let res = BypassScenario {
        checker: Some(allowlist(&["someoneelse"])),
        client_identities: user_identities(&["client_user"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

/// On the client-identities path, a request with no client identities falls back
/// to the author, who is also not in the empty allowlist, so the bypass fails
/// closed.
#[mononoke::fbinit_test]
async fn test_bypass_with_empty_client_identities_fails_closed(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(allowlist(&[])),
        client_identities: MononokeIdentitySet::new(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
}

// =========================================================================
// Agent bypass tests
//
// A bypass restricted by a permission group is a human judgement call, so an
// agent must not be able to use one even when it pushes with an identity that
// is in the group. These are fbcode-only: the OSS `likely_an_agent` is a
// constant `false` stub, so there is no agent to test with there.
// =========================================================================

/// The agent/human identity builders really do differ in what
/// `likely_an_agent()` reports, so the scenarios below are not vacuous.
#[cfg(fbcode_build)]
#[mononoke::test]
fn test_agent_identities_are_recognised_as_an_agent() {
    use permission_checker::MononokeIdentitySetExt;

    assert!(agent_identities("client_user").likely_an_agent());
    assert!(!user_identities(&["client_user"]).likely_an_agent());
}

/// An agent whose client identity is in the bypass permission group is still
/// refused: only a human should be able to decide to override a restricted hook.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_agent_in_bypass_group_cannot_bypass(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(AlwaysMember::new().into()),
        client_identities: agent_identities("client_user"),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_agent_bypass_rejected(&res);
}

/// The same on the changeset-author path, which is the path a real push takes by
/// default. Worth its own test: that path membership-checks the author's
/// synthetic `USER:<unixname>` identity, which never carries the credential
/// attributes `likely_an_agent()` looks for, so the agent signal has to come
/// from the pusher's own metadata rather than the identity set being checked.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_agent_cannot_bypass_on_author_path(fb: FacebookInit) {
    let res = BypassScenario {
        // `changeset_with_bypass_msg` is authored by "Test User <test@fb.com>".
        checker: Some(allowlist(&["test"])),
        client_identities: agent_identities("client_user"),
        jk_use_client_identities: false,
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_agent_bypass_rejected(&res);
}

/// An agent that is not in the permission group is rejected for the more
/// specific reason: it is not a member. The group check comes first, so the
/// agent never sees a message about being an agent when the group membership is
/// what it actually lacks.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_agent_not_in_bypass_group_gets_group_note(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(NeverMember::new().into()),
        client_identities: agent_identities("client_user"),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
    let info = res[0]
        .get_execution()
        .rejection_info()
        .expect("hook rejected");
    assert!(
        info.long_description.contains("not a member of group"),
        "expected the not-a-member note, got {:?}",
        info.long_description,
    );
    assert!(
        !info.long_description.contains("cannot be made by an agent"),
        "being an agent is not why this was refused, got {:?}",
        info.long_description,
    );
}

/// A human in the bypass permission group keeps bypassing: the block targets
/// agents only.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_human_in_bypass_group_still_bypasses(fb: FacebookInit) {
    let res = BypassScenario {
        checker: Some(AlwaysMember::new().into()),
        client_identities: user_identities(&["client_user"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// A hook that sets `allow_agent_bypass` accepts the bypass from an agent that
/// is in the group; the refusal above is the default, not a rule.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_agent_in_bypass_group_bypasses_when_hook_allows_agents(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: bypass_config_with_group_allowing_agents(),
        checker: Some(AlwaysMember::new().into()),
        client_identities: agent_identities("client_user"),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

// =========================================================================
// Implicit group bypass
//
// With `bypass_implicit_for_group`, membership in the permission group is the
// whole authorization: no marker in the commit message or pushvars is needed.
// =========================================================================

/// A member pushing a changeset with no bypass marker is bypassed.
#[mononoke::fbinit_test]
async fn test_implicit_bypass_member_without_marker(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: implicit_bypass_config_with_group(),
        checker: Some(AlwaysMember::new().into()),
        changeset: default_changeset(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_bypassed(&res);
}

/// A non-member pushing without a marker: the hook runs, and the rejection says
/// the group is what they lack.
#[mononoke::fbinit_test]
async fn test_implicit_bypass_non_member_gets_group_note(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: implicit_bypass_config_with_group(),
        checker: Some(NeverMember::new().into()),
        changeset: default_changeset(),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_rejected(&res);
    let info = res[0]
        .get_execution()
        .rejection_info()
        .expect("hook rejected");
    assert!(
        info.long_description.contains("not a member of group"),
        "expected the not-a-member note, got {:?}",
        info.long_description,
    );
}

// =========================================================================
// Bypass permission group test helpers
// =========================================================================

/// Pushvars that trigger the `BYPASS=true` pushvar bypass.
fn bypass_pushvars() -> HashMap<String, bytes::Bytes> {
    hashmap! { "BYPASS".to_string() => bytes::Bytes::from("true") }
}

/// A permission checker that records how many times it is consulted, so tests can
/// assert the membership check ran the expected number of times.
struct CountingMember {
    is_member: bool,
    calls: Arc<AtomicUsize>,
}

impl CountingMember {
    /// Returns the checker plus a handle to read its call count.
    fn new(is_member: bool) -> (ArcMembershipChecker, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let checker: ArcMembershipChecker = Arc::new(Self {
            is_member,
            calls: calls.clone(),
        });
        (checker, calls)
    }
}

#[async_trait]
impl MembershipChecker for CountingMember {
    async fn is_member(&self, _identities: &MononokeIdentitySet) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.is_member
    }
}

/// A `MemberAllowlist` permission checker admitting exactly the given unixnames
/// (as `USER:<unixname>` identities). Pass an empty slice for a checker that
/// admits no one (used to exercise the fail-closed path).
fn allowlist(unixnames: &[&str]) -> ArcMembershipChecker {
    let identities: MononokeIdentitySet = unixnames
        .iter()
        .map(|name| MononokeIdentity::from_legacy_type_data("USER", *name))
        .collect();
    MemberAllowlist::new(identities).into()
}

/// Assert an authorized (group-gated) bypass was honored: the hook ran and
/// rejected, but the rejection was folded into a single accepted outcome (so the
/// push proceeds).
fn assert_bypassed(outcomes: &[HookOutcome]) {
    assert_eq!(
        outcomes.len(),
        1,
        "expected one folded outcome for the authorized bypass, got {outcomes:?}",
    );
    assert!(
        outcomes[0].get_execution().is_accepted(),
        "expected the authorized bypass to fold into an accepted outcome, got {:?}",
        outcomes[0].get_execution(),
    );
}

/// Assert a group-less bypass was honored by skipping the hook before it ran:
/// the hook never produced an outcome, so the outcome list is empty.
fn assert_skipped_pre_run(outcomes: &[HookOutcome]) {
    assert!(
        outcomes.is_empty(),
        "expected the group-less bypass to skip the hook pre-run, got: {outcomes:?}",
    );
}

/// Assert the hook ran and accepted (e.g. an unauthorized bypass was ignored but
/// the hook had nothing to object to).
fn assert_hook_accepted(outcomes: &[HookOutcome]) {
    assert_eq!(
        outcomes.len(),
        1,
        "expected the hook to run, got {} outcome(s)",
        outcomes.len(),
    );
    assert!(
        outcomes[0].get_execution().is_accepted(),
        "expected the hook to accept, got {:?}",
        outcomes[0].get_execution(),
    );
}

/// Assert the bypass was NOT honored: the always-rejecting hook ran and rejected.
fn assert_hook_rejected(outcomes: &[HookOutcome]) {
    assert_eq!(
        outcomes.len(),
        1,
        "expected the hook to run, got {} outcome(s)",
        outcomes.len(),
    );
    assert!(
        outcomes[0].get_execution().is_rejected(),
        "expected the hook to reject, got {:?}",
        outcomes[0].get_execution(),
    );
}

/// Assert the bypass was refused because the pusher is an agent: the hook ran
/// and rejected, annotated with the note handing the decision to a human.
#[cfg(fbcode_build)]
fn assert_agent_bypass_rejected(outcomes: &[HookOutcome]) {
    assert_hook_rejected(outcomes);
    let info = outcomes[0]
        .get_execution()
        .rejection_info()
        .expect("hook rejected");
    assert!(
        info.long_description.contains("cannot be made by an agent"),
        "expected the agent bypass note, got {:?}",
        info.long_description,
    );
}

/// A single bypass-permission-group scenario. Registers one changeset hook
/// ("hook1", default: always-rejecting) on bookmark "bm1" with `bypass_config` +
/// `checker`, then runs it over `changeset` under the given JustKnobs and client
/// identities, and returns the hook outcomes.
///
/// Construct it with struct-update syntax so each test overrides only the
/// fields relevant to its scenario, e.g.
/// `BypassScenario { checker: Some(AlwaysMember::new().into()), ..Default::default() }`.
///
/// Defaults: a permission-group bypass config, an always-rejecting hook, no
/// checker, a changeset carrying the bypass message, no pushvars, no client
/// identities, and the client-identities path selected.
struct BypassScenario {
    bypass_config: HookConfig,
    hook: Box<dyn ChangesetHook>,
    checker: Option<ArcMembershipChecker>,
    changeset: BonsaiChangeset,
    pushvars: Option<HashMap<String, bytes::Bytes>>,
    client_identities: MononokeIdentitySet,
    jk_use_client_identities: bool,
}

impl Default for BypassScenario {
    fn default() -> Self {
        Self {
            bypass_config: bypass_config_with_group(),
            hook: always_rejecting_changeset_hook(),
            checker: None,
            changeset: changeset_with_bypass_msg(),
            pushvars: None,
            client_identities: MononokeIdentitySet::new(),
            jk_use_client_identities: true,
        }
    }
}

impl BypassScenario {
    async fn run(self, fb: FacebookInit) -> Vec<HookOutcome> {
        let ctx = ctx_with_identities(fb, self.client_identities);

        let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
        hook_manager.register_changeset_hook(
            "hook1",
            self.hook,
            self.bypass_config,
            self.checker.map(bypass_checkers).unwrap_or_default(),
        );
        let bm = BookmarkKey::new("bm1").unwrap();
        hook_manager.set_hooks_for_bookmark(bm.clone().into(), vec!["hook1".to_string()]);

        let changesets = [self.changeset];
        justknobs::test_helpers::with_just_knobs_async(
            bypass_permission_groups_jk(self.jk_use_client_identities),
            Box::pin(hook_manager.run_changesets_hooks_for_bookmark(
                &ctx,
                &changesets,
                &bm,
                self.pushvars.as_ref(),
                CrossRepoPushSource::NativeToThisRepo,
                PushAuthoredBy::User,
                HookExecutionPurpose::LandAttempt,
                None,
                LogOnlyRejections::Suppress,
            )),
        )
        .await
        .unwrap()
    }
}

// =========================================================================
// Bypass identity-check provenance
//
// A denied bypass names the required group but not whose membership was
// actually tested, so these pin the `CheckedBypassIdentities` that backs the
// `bypass_identity_source` / `bypass_identities_checked` Scuba columns.
// =========================================================================

/// Resolve the eager bypass decision for one always-rejecting hook registered
/// with a group-gated commit-message bypass, under the given JustKnobs path.
async fn bypass_decision(
    fb: FacebookInit,
    client_identities: MononokeIdentitySet,
    checker: Option<ArcMembershipChecker>,
    author: &str,
    use_client_identities: bool,
) -> BypassDecision {
    let ctx = ctx_with_identities(fb, client_identities);
    let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
    hook_manager.register_changeset_hook(
        "hook1",
        always_rejecting_changeset_hook(),
        bypass_config_with_group(),
        checker.map(bypass_checkers).unwrap_or_default(),
    );

    justknobs::test_helpers::with_just_knobs_async(
        bypass_permission_groups_jk(use_client_identities),
        Box::pin(hook_manager.compute_bypass_decision_for_test(
            "hook1",
            &ctx,
            None,
            Some("This commit has @bypass_hook in the message"),
            Some(author),
        )),
    )
    .await
    .unwrap()
}

/// The recorded provenance is that of the check that actually decided. On the
/// client-identities path a *miss* is not the decision -- it falls through to
/// the commit author -- so a denial there records `CommitAuthor`, the check that
/// produced it, and the author's identity rather than the pusher's.
#[mononoke::fbinit_test]
async fn test_bypass_check_records_the_deciding_check(fb: FacebookInit) {
    let decision = bypass_decision(
        fb,
        user_identities(&["alice"]),
        Some(allowlist(&[])),
        "Test User <test@fb.com>",
        true,
    )
    .await;

    match decision {
        BypassDecision::UnauthorizedUser {
            reason,
            group,
            check,
            ..
        } => {
            assert_eq!(group, "test_bypass_group");
            assert!(
                !reason.is_empty(),
                "the bypass reason backs the `bypass_reason` column on denials",
            );
            assert_eq!(check.source, BypassIdentitySource::CommitAuthor);
            assert_eq!(check.identities, vec!["USER:test".to_string()]);
        }
        other => panic!("expected UnauthorizedUser, got {other:?}"),
    }
}

/// On the commit-author path, a denied bypass records the identity derived from
/// the author -- not the pusher's.
#[mononoke::fbinit_test]
async fn test_bypass_check_records_commit_author_source(fb: FacebookInit) {
    let decision = bypass_decision(
        fb,
        user_identities(&["alice"]),
        Some(allowlist(&[])),
        "Test User <test@fb.com>",
        false,
    )
    .await;

    match decision {
        BypassDecision::UnauthorizedUser { check, .. } => {
            assert_eq!(check.source, BypassIdentitySource::CommitAuthor);
            assert_eq!(check.identities, vec!["USER:test".to_string()]);
        }
        other => panic!("expected UnauthorizedUser, got {other:?}"),
    }
}

/// An unparsable author (a Sandcastle container's `root@<host>` default) makes
/// the author path silently fall back to the pusher's identities. That is the
/// case this provenance exists to make visible: the group named in the
/// rejection was never checked against the author at all.
#[mononoke::fbinit_test]
async fn test_bypass_check_records_author_fallback_to_client(fb: FacebookInit) {
    let decision = bypass_decision(
        fb,
        user_identities(&["svcscm"]),
        Some(allowlist(&[])),
        "root@0ed7-b93c-0004-0000.twshared29495.01.snb2.tw.fbinfra.net",
        false,
    )
    .await;

    match decision {
        BypassDecision::UnauthorizedUser { check, .. } => {
            assert_eq!(
                check.source,
                BypassIdentitySource::CommitAuthorFallbackToClient,
            );
            assert_eq!(check.identities, vec!["USER:svcscm".to_string()]);
        }
        other => panic!("expected UnauthorizedUser, got {other:?}"),
    }
}

/// An authorized bypass carries the same provenance, so accepted and denied
/// bypasses are comparable in one query.
#[mononoke::fbinit_test]
async fn test_authorized_bypass_records_identity_check(fb: FacebookInit) {
    let decision = bypass_decision(
        fb,
        user_identities(&["alice"]),
        Some(allowlist(&["alice"])),
        "Test User <test@fb.com>",
        true,
    )
    .await;

    match decision {
        BypassDecision::Authorized { check, .. } => {
            let check = check.expect("a membership check ran");
            assert_eq!(check.source, BypassIdentitySource::ClientIdentities);
            assert_eq!(check.identities, vec!["USER:alice".to_string()]);
        }
        other => panic!("expected Authorized, got {other:?}"),
    }
}

/// With no membership checker registered for the group, nobody's membership
/// can be checked, so the bypass is refused and the identity source records
/// why.
#[mononoke::fbinit_test]
async fn test_bypass_without_checker_is_refused(fb: FacebookInit) {
    let decision = bypass_decision(
        fb,
        user_identities(&["alice"]),
        None,
        "Test User <test@fb.com>",
        true,
    )
    .await;

    match decision {
        BypassDecision::UnauthorizedUser { check, .. } => {
            assert_eq!(check.source, BypassIdentitySource::NoChecker);
            assert!(check.identities.is_empty());
        }
        other => panic!("expected UnauthorizedUser, got {other:?}"),
    }
}

/// End-to-end: an author whose commit email
/// local-part (`jdoe`) differs from their unixname (`johndoe`) can bypass a
/// group-gated hook when they are in the bypass group under their unixname,
/// because the EmployeeService resolves the author email to the canonical
/// unixname. EmployeeService is fbcode-only.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_bypass_resolves_author_email_to_group_unixname(fb: FacebookInit) {
    use employee_service::MononokeEmployee;
    use employee_service::MononokeEmployeeService;
    use employee_service::mock::MockEmployeeService;

    // jdoe@example.com -> uid 7 -> unixname "johndoe".
    let service: Arc<dyn MononokeEmployeeService + Send + Sync> =
        Arc::new(MockEmployeeService::new_with_employees(vec![
            MononokeEmployee {
                unixname: "johndoe".to_string(),
                uid: 7,
                email: "jdoe@example.com".to_string(),
                fbid: 0,
                manager_fbid: 0,
                is_employed: true,
            },
        ]));

    let changeset = BonsaiChangesetMut {
        author: "John Doe <jdoe@example.com>".to_string(),
        author_date: DateTime::from_timestamp(1584887580, 0).expect("Getting timestamp"),
        message: "This commit has @bypass_hook in the message".to_string(),
        file_changes: sorted_vector_map! {
            to_mpath("dir1/file.txt") => FileChange::tracked(ONES_CTID, FileType::Regular, 10, None, GitLfs::FullContent),
        },
        ..Default::default()
    }
    .freeze()
    .expect("Created changeset");

    let ctx = ctx_with_identities(fb, MononokeIdentitySet::new());
    let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
    hook_manager.set_employee_service_for_test(service);
    // Group admits the unixname "johndoe", NOT the email local-part "jdoe".
    hook_manager.register_changeset_hook(
        "hook1",
        always_rejecting_changeset_hook(),
        bypass_config_with_group(),
        bypass_checkers(allowlist(&["johndoe"])),
    );
    let bm = BookmarkKey::new("bm1").unwrap();
    hook_manager.set_hooks_for_bookmark(bm.clone().into(), vec!["hook1".to_string()]);

    // The changeset-author path selected, and the EmployeeService
    // canonical-unixname resolution enabled. No client identities on the request,
    // so the author's identity drives the check.
    let jk = justknobs::test_helpers::JustKnobsInMemory::new(
        [
            (
                "scm/mononoke:check_hook_bypass_permission_group_with_client_identities"
                    .to_string(),
                justknobs::test_helpers::KnobVal::Bool(false),
            ),
            (
                "scm/mononoke:resolve_unixname_from_employee_service_for_hook_bypass".to_string(),
                justknobs::test_helpers::KnobVal::Bool(true),
            ),
        ]
        .into(),
    );

    let changesets = [changeset];
    let outcomes = justknobs::test_helpers::with_just_knobs_async(
        jk,
        Box::pin(hook_manager.run_changesets_hooks_for_bookmark(
            &ctx,
            &changesets,
            &bm,
            None,
            CrossRepoPushSource::NativeToThisRepo,
            PushAuthoredBy::User,
            HookExecutionPurpose::LandAttempt,
            None,
            LogOnlyRejections::Suppress,
        )),
    )
    .await
    .unwrap();

    assert_bypassed(&outcomes);
}

// =========================================================================
// Log-only hooks: the bypass decision is recorded, not applied
// =========================================================================

fn normal(value: &str) -> ScubaValue {
    ScubaValue::Normal(value.to_string())
}

fn checked_identities() -> CheckedBypassIdentities {
    CheckedBypassIdentities {
        source: BypassIdentitySource::ClientIdentities,
        identities: vec!["USER:alice".to_string()],
    }
}

/// Assert the identity-check columns `checked_identities()` produces are set.
fn assert_identity_check_columns(scuba: &MononokeScubaSampleBuilder) {
    assert_eq!(
        scuba.get("bypass_identity_source"),
        Some(&normal("client_identities"))
    );
    assert_eq!(
        scuba.get("bypass_identities_checked"),
        Some(&ScubaValue::NormVector(vec!["USER:alice".to_string()]))
    );
}

/// Run `add_log_only_bypass_columns` for `bypass` on a fresh sample.
fn log_only_bypass_columns(bypass: &BypassDecision) -> MononokeScubaSampleBuilder {
    let mut scuba = MononokeScubaSampleBuilder::with_discard();
    add_log_only_bypass_columns(&mut scuba, bypass);
    scuba
}

#[mononoke::test]
fn test_log_only_bypass_columns_authorized() {
    let scuba = log_only_bypass_columns(&BypassDecision::Authorized {
        condition: 0,
        reason: "bypass string: @bypass_hook".to_string(),
        permission_group: Some("test_bypass_group".to_string()),
        check: Some(checked_identities()),
    });
    assert_eq!(
        scuba.get("log_only_bypass_decision"),
        Some(&normal("authorized"))
    );
    assert_eq!(
        scuba.get("bypass_reason"),
        Some(&normal("bypass string: @bypass_hook"))
    );
    assert_eq!(
        scuba.get("bypass_permission_group"),
        Some(&normal("test_bypass_group"))
    );
    assert_identity_check_columns(&scuba);
    assert_eq!(scuba.get("bypass_blocked_for_agent"), None);
}

#[mononoke::test]
fn test_log_only_bypass_columns_authorized_without_group() {
    let scuba = log_only_bypass_columns(&BypassDecision::Authorized {
        condition: 0,
        reason: "bypass string: @bypass_hook".to_string(),
        permission_group: None,
        check: None,
    });
    assert_eq!(
        scuba.get("log_only_bypass_decision"),
        Some(&normal("authorized"))
    );
    assert_eq!(
        scuba.get("bypass_reason"),
        Some(&normal("bypass string: @bypass_hook"))
    );
    assert_eq!(scuba.get("bypass_permission_group"), None);
    assert_eq!(scuba.get("bypass_identity_source"), None);
    assert_eq!(scuba.get("bypass_identities_checked"), None);
    assert_eq!(scuba.get("bypass_blocked_for_agent"), None);
}

#[mononoke::test]
fn test_log_only_bypass_columns_unauthorized_user() {
    let scuba = log_only_bypass_columns(&BypassDecision::UnauthorizedUser {
        condition: 0,
        reason: "bypass string: @bypass_hook".to_string(),
        group: "test_bypass_group".to_string(),
        check: checked_identities(),
    });
    assert_eq!(
        scuba.get("log_only_bypass_decision"),
        Some(&normal("unauthorized_user"))
    );
    assert_eq!(
        scuba.get("bypass_reason"),
        Some(&normal("bypass string: @bypass_hook"))
    );
    assert_eq!(
        scuba.get("bypass_permission_group"),
        Some(&normal("test_bypass_group"))
    );
    assert_identity_check_columns(&scuba);
    assert_eq!(scuba.get("bypass_blocked_for_agent"), None);
}

#[mononoke::test]
fn test_log_only_bypass_columns_unauthorized_agent() {
    let scuba = log_only_bypass_columns(&BypassDecision::UnauthorizedAgent {
        condition: 0,
        reason: "bypass string: @bypass_hook".to_string(),
        permission_group: Some("test_bypass_group".to_string()),
        check: Some(checked_identities()),
    });
    assert_eq!(
        scuba.get("log_only_bypass_decision"),
        Some(&normal("unauthorized_agent"))
    );
    assert_eq!(
        scuba.get("bypass_reason"),
        Some(&normal("bypass string: @bypass_hook"))
    );
    assert_eq!(
        scuba.get("bypass_permission_group"),
        Some(&normal("test_bypass_group"))
    );
    assert_eq!(scuba.get("bypass_blocked_for_agent"), Some(&normal("true")));
    assert_identity_check_columns(&scuba);
}

#[mononoke::test]
fn test_log_only_bypass_columns_none() {
    let scuba = log_only_bypass_columns(&BypassDecision::NoBypass);
    assert_eq!(scuba.get("log_only_bypass_decision"), Some(&normal("none")));
    for column in [
        "bypass_reason",
        "bypass_permission_group",
        "bypass_identity_source",
        "bypass_identities_checked",
        "bypass_blocked_for_agent",
    ] {
        assert_eq!(scuba.get(column), None, "{column} must stay unset");
    }
}

fn log_only_bypass_config_with_group() -> HookConfig {
    HookConfig {
        log_only: true,
        ..bypass_config_with_group()
    }
}

/// The eager bypass decision is computed for log-only hooks too: the manager
/// does not look at `log_only`, so a log-only rollout records the same decision
/// the hook would apply once enforcing.
#[mononoke::fbinit_test]
async fn test_log_only_hook_gets_a_bypass_decision(fb: FacebookInit) {
    let ctx = ctx_with_identities(fb, user_identities(&["alice"]));
    let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
    hook_manager.register_changeset_hook(
        "hook1",
        always_rejecting_changeset_hook(),
        log_only_bypass_config_with_group(),
        bypass_checkers(AlwaysMember::new().into()),
    );

    let decision = justknobs::test_helpers::with_just_knobs_async(
        bypass_permission_groups_jk(true),
        Box::pin(hook_manager.compute_bypass_decision_for_test(
            "hook1",
            &ctx,
            None,
            Some("This commit has @bypass_hook in the message"),
            Some("Test User <test@fb.com>"),
        )),
    )
    .await
    .unwrap();

    assert!(
        matches!(decision, BypassDecision::Authorized { .. }),
        "expected Authorized, got {decision:?}",
    );
}

/// A log-only hook with a group-gated bypass and an authorized pusher: log-only
/// mode still turns the rejection into an accepted execution. Whether the
/// bypass would have applied is only recorded, never applied.
#[mononoke::fbinit_test]
async fn test_log_only_hook_with_authorized_bypass_is_accepted(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: log_only_bypass_config_with_group(),
        checker: Some(AlwaysMember::new().into()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_accepted(&res);
}

/// The same with a pusher outside the group: log-only mode never blocks, so
/// the denied bypass changes nothing about the outcome.
#[mononoke::fbinit_test]
async fn test_log_only_hook_with_unauthorized_bypass_is_accepted(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: log_only_bypass_config_with_group(),
        checker: Some(NeverMember::new().into()),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_hook_accepted(&res);
}

// =========================================================================
// Bypass conditions: any condition may match, and each names who may use it
// =========================================================================

/// Resolve the eager bypass decision for one always-rejecting hook with
/// `config`, for a push by `client_identities` of a commit with `cs_msg`
/// authored by "Test User <test@fb.com>".
async fn decision_for(
    fb: FacebookInit,
    config: HookConfig,
    checkers: BypassCheckers,
    client_identities: MononokeIdentitySet,
    pushvars: Option<&HashMap<String, bytes::Bytes>>,
    cs_msg: &str,
) -> BypassDecision {
    let ctx = ctx_with_identities(fb, client_identities);
    let mut hook_manager = setup_hook_manager(fb, hashmap! {}, hashmap! {}).await;
    hook_manager.register_changeset_hook(
        "hook1",
        always_rejecting_changeset_hook(),
        config,
        checkers,
    );
    justknobs::test_helpers::with_just_knobs_async(
        bypass_permission_groups_jk(true),
        Box::pin(hook_manager.compute_bypass_decision_for_test(
            "hook1",
            &ctx,
            pushvars,
            Some(cs_msg),
            Some("Test User <test@fb.com>"),
        )),
    )
    .await
    .unwrap()
}

/// Assert the hook ran and rejected, with `note` added to its rejection.
fn assert_rejected_with_note(outcomes: &[HookOutcome], note: &str) {
    assert_hook_rejected(outcomes);
    let info = outcomes[0]
        .get_execution()
        .rejection_info()
        .expect("hook rejected");
    assert!(
        info.long_description.contains(note),
        "expected the note {note:?}, got {:?}",
        info.long_description,
    );
}

/// A pushvar for humans in the group, or an allowlisted agent: the agent
/// matches the second condition without the pushvar or the group, and the
/// decision names that condition.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_any_bypass_condition_may_match(fb: FacebookInit) {
    let agent = agent_identities("client_user");
    let config = HookConfig {
        bypasses: vec![
            HookBypassCondition::pushvar("BYPASS", "true").with_permission_group(BYPASS_GROUP),
            HookBypassCondition::identities(agent.clone()).with_actor(HookBypassActor::Agent),
        ],
        ..Default::default()
    };

    let decision = decision_for(
        fb,
        config.clone(),
        bypass_checkers(NeverMember::new().into()),
        agent,
        None,
        "no marker",
    )
    .await;
    assert!(
        matches!(decision, BypassDecision::Authorized { condition: 1, .. }),
        "expected the second condition to authorize, got {decision:?}",
    );

    // A human in the group with the pushvar matches the first condition.
    let decision = decision_for(
        fb,
        config,
        bypass_checkers(AlwaysMember::new().into()),
        user_identities(&["client_user"]),
        Some(&bypass_pushvars()),
        "no marker",
    )
    .await;
    assert!(
        matches!(decision, BypassDecision::Authorized { condition: 0, .. }),
        "expected the first condition to authorize, got {decision:?}",
    );
}

/// A condition without a group is restricted to humans by default: a human
/// with the marker skips the hook, an agent with the marker is told the bypass
/// is not for agents.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn test_human_only_condition_refuses_agent(fb: FacebookInit) {
    let config = HookConfig {
        bypasses: vec![HookBypassCondition::commit_message("@bypass_hook")],
        ..Default::default()
    };

    let res = BypassScenario {
        bypass_config: config.clone(),
        client_identities: user_identities(&["client_user"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_skipped_pre_run(&res);

    let res = BypassScenario {
        bypass_config: config,
        client_identities: agent_identities("client_user"),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_rejected_with_note(&res, "this bypass cannot be used by an agent");
}

/// A condition restricted to agents is refused to a human.
#[mononoke::fbinit_test]
async fn test_agent_only_condition_refuses_human(fb: FacebookInit) {
    let res = BypassScenario {
        bypass_config: HookConfig {
            bypasses: vec![
                HookBypassCondition::commit_message("@bypass_hook")
                    .with_actor(HookBypassActor::Agent),
            ],
            ..Default::default()
        },
        client_identities: user_identities(&["client_user"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_rejected_with_note(&res, "it can only be used by an agent");
}

/// An identity allowlist with no marker bypasses the hook for the identities on
/// it and no one else.
#[mononoke::fbinit_test]
async fn test_identity_condition(fb: FacebookInit) {
    let config = HookConfig {
        bypasses: vec![
            HookBypassCondition::identities(user_identities(&["alice"]))
                .with_actor(HookBypassActor::Any),
        ],
        ..Default::default()
    };

    let res = BypassScenario {
        bypass_config: config.clone(),
        changeset: default_changeset(),
        client_identities: user_identities(&["alice"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_skipped_pre_run(&res);

    let res = BypassScenario {
        bypass_config: config,
        changeset: default_changeset(),
        client_identities: user_identities(&["bob"]),
        ..Default::default()
    }
    .run(fb)
    .await;
    assert_rejected_with_note(&res, "not one of the identities allowed to use it");
}

/// When no condition matches, the decision describes the first condition whose
/// markers are present.
#[mononoke::fbinit_test]
async fn test_near_miss_is_first_triggered_condition(fb: FacebookInit) {
    let config = HookConfig {
        bypasses: vec![
            HookBypassCondition::pushvar("BYPASS", "true").with_permission_group(BYPASS_GROUP),
            HookBypassCondition::commit_message("@bypass_hook").with_actor(HookBypassActor::Agent),
            HookBypassCondition::commit_message("@bypass_hook").with_permission_group(BYPASS_GROUP),
        ],
        ..Default::default()
    };
    let decision = decision_for(
        fb,
        config,
        bypass_checkers(NeverMember::new().into()),
        user_identities(&["client_user"]),
        None,
        "This commit has @bypass_hook in the message",
    )
    .await;
    assert!(
        matches!(
            decision,
            BypassDecision::UnauthorizedHuman {
                condition: 1,
                permission_group: None,
                ..
            }
        ),
        "expected the agent-only condition, got {decision:?}",
    );
}

/// Conditions that share a permission group share its membership check.
#[mononoke::fbinit_test]
async fn test_shared_group_is_checked_once(fb: FacebookInit) {
    let (checker, calls) = CountingMember::new(false);
    let decision = decision_for(
        fb,
        HookConfig {
            bypasses: vec![
                HookBypassCondition::pushvar("BYPASS", "true").with_permission_group(BYPASS_GROUP),
                HookBypassCondition::commit_message("@bypass_hook")
                    .with_permission_group(BYPASS_GROUP),
            ],
            ..Default::default()
        },
        bypass_checkers(checker),
        user_identities(&["client_user"]),
        Some(&bypass_pushvars()),
        "This commit has @bypass_hook in the message",
    )
    .await;
    assert!(
        matches!(
            decision,
            BypassDecision::UnauthorizedUser { condition: 0, .. }
        ),
        "expected the first condition's group to deny, got {decision:?}",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one membership decision (client identities, then the commit author) \
         serves both conditions",
    );
}

#[mononoke::test]
fn test_bypass_columns_name_the_condition() {
    let scuba = log_only_bypass_columns(&BypassDecision::Authorized {
        condition: 2,
        reason: "implicit: allowed identity".to_string(),
        permission_group: None,
        check: None,
    });
    assert_eq!(scuba.get("bypass_condition"), Some(&ScubaValue::Int(2)));
}

#[mononoke::test]
fn test_log_only_bypass_columns_unauthorized_human() {
    let scuba = log_only_bypass_columns(&BypassDecision::UnauthorizedHuman {
        condition: 1,
        reason: "bypass string: @bypass_hook".to_string(),
        permission_group: None,
        check: None,
    });
    assert_eq!(
        scuba.get("log_only_bypass_decision"),
        Some(&normal("unauthorized_human"))
    );
    assert_eq!(scuba.get("bypass_condition"), Some(&ScubaValue::Int(1)));
    assert_eq!(scuba.get("bypass_blocked_for_human"), Some(&normal("true")));
    assert_eq!(scuba.get("bypass_blocked_for_agent"), None);
}

#[mononoke::test]
fn test_log_only_bypass_columns_unauthorized_identity() {
    let scuba = log_only_bypass_columns(&BypassDecision::UnauthorizedIdentity {
        condition: 0,
        reason: "implicit: allowed identity".to_string(),
        permission_group: None,
        check: None,
    });
    assert_eq!(
        scuba.get("log_only_bypass_decision"),
        Some(&normal("unauthorized_identity"))
    );
    assert_eq!(
        scuba.get("bypass_identity_not_allowed"),
        Some(&normal("true"))
    );
}

/// A condition's permission group is checked against the commit's author,
/// while its identities are matched against the pusher: here the author is in
/// the group and only the pusher is allowlisted.
#[mononoke::fbinit_test]
async fn test_group_and_identities_check_author_and_pusher(fb: FacebookInit) {
    // `decision_for` commits are authored by "Test User <test@fb.com>".
    let config = HookConfig {
        bypasses: vec![HookBypassCondition {
            permission_group: Some(BYPASS_GROUP.to_string()),
            ..HookBypassCondition::identities(user_identities(&["client_user"]))
        }],
        ..Default::default()
    };

    let decision = decision_for(
        fb,
        config.clone(),
        bypass_checkers(allowlist(&["test"])),
        user_identities(&["client_user"]),
        None,
        "no marker",
    )
    .await;
    match decision {
        BypassDecision::Authorized {
            check: Some(check), ..
        } => assert_eq!(check.source, BypassIdentitySource::CommitAuthor),
        other => panic!("expected Authorized by the author's membership, got {other:?}"),
    }

    // A pusher that is not allowlisted is refused, although the author is in
    // the group.
    let decision = decision_for(
        fb,
        config,
        bypass_checkers(allowlist(&["test"])),
        user_identities(&["someone_else"]),
        None,
        "no marker",
    )
    .await;
    assert!(
        matches!(decision, BypassDecision::UnauthorizedIdentity { .. }),
        "expected UnauthorizedIdentity, got {decision:?}",
    );
}
