/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use context::CoreContext;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
pub use restricted_paths::DenyingRestriction;
use restricted_paths::PathRestrictionInfo;
use restricted_paths::PermissionRequestGroup;
use source_control::PartialOptions;

/// Access check result for a restricted path.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PathAccessInfo {
    /// Core restriction info from the restricted_paths crate.
    pub restriction: PathRestrictionInfo,

    /// Whether the caller has access. None if not checked.
    pub has_access: Option<bool>,
}

impl PathAccessInfo {
    /// Convenience accessor for the restriction root.
    pub fn restriction_root(&self) -> &NonRootMPath {
        &self.restriction.restriction_root
    }

    /// Convenience accessor for the repo region ACL.
    pub fn repo_region_acl(&self) -> &str {
        &self.restriction.repo_region_acl
    }

    /// Convenience accessor for the permission request group.
    pub fn permission_request_group(&self) -> &PermissionRequestGroup {
        &self.restriction.permission_request_group
    }
}

/// Lookup behavior for a changed-path restriction comparison.
#[derive(Clone, Copy, Debug)]
pub struct ChangedPathsRestrictionOptions {
    /// Populate complete-side and per-restriction access results.
    pub check_permissions: bool,
    /// Compare effective policies, including paths without a file on one side.
    pub include_permission_changes: bool,
    /// Cover both endpoint and subtree-source-relative views of parent comparisons.
    pub include_subtree_source_changes: bool,
}

/// Effective restrictions and access at a path, independent of file existence.
#[derive(Clone, Debug, PartialEq)]
pub struct PathPolicyInfo {
    /// All restrictions matching this path, retaining their full identities.
    pub restrictions: Vec<PathAccessInfo>,
    /// Complete-side access, or None when permission checking was not requested.
    pub has_access: Option<bool>,
}

/// A copy source's metadata at its actual revision and path.
#[derive(Clone, Debug, PartialEq)]
pub struct PathRestrictionCopySourceInfo {
    pub commit_id: ChangesetId,
    pub path: NonRootMPath,
    pub info: PathPolicyInfo,
}

/// One declared source edge. File paths are mapped component-wise from the destination.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SubtreeSourceMapping {
    pub commit_id: ChangesetId,
    pub source_prefix: MPath,
    pub destination_prefix: MPath,
}

/// Effective policy on a declared subtree-copy source edge.
#[derive(Clone, Debug, PartialEq)]
pub struct SubtreeSourcePolicyInfo {
    pub mapping: SubtreeSourceMapping,
    pub info: PathPolicyInfo,
}

/// Literal file sides of a path changed in either comparison view.
#[derive(Clone, Debug, PartialEq)]
pub enum ChangedPathRestrictionSides {
    Added(PathPolicyInfo),
    Removed(PathPolicyInfo),
    Changed {
        base: PathPolicyInfo,
        target: PathPolicyInfo,
    },
    /// No net endpoint change; the declared subtree source differs from target.
    Unchanged {
        base: PathPolicyInfo,
        target: PathPolicyInfo,
    },
    /// A subtree-source file is absent from both literal endpoints.
    SourceOnly(SubtreeSourcePolicyInfo),
}

impl ChangedPathRestrictionSides {
    pub fn base(&self) -> Option<&PathPolicyInfo> {
        match self {
            Self::Added(_) | Self::SourceOnly(_) => None,
            Self::Removed(base) | Self::Changed { base, .. } | Self::Unchanged { base, .. } => {
                Some(base)
            }
        }
    }

    pub fn target(&self) -> Option<&PathPolicyInfo> {
        match self {
            Self::Removed(_) | Self::SourceOnly(_) => None,
            Self::Added(target) | Self::Changed { target, .. } | Self::Unchanged { target, .. } => {
                Some(target)
            }
        }
    }
}

/// A policy difference at a changed path, including interesting copy sources.
#[derive(Clone, Debug, PartialEq)]
pub struct PathPermissionChangeInfo {
    /// Effective policy at the base revision, even when the file is absent.
    pub base: PathPolicyInfo,
    /// Effective policy at the target revision, even when the file is absent.
    pub target: PathPolicyInfo,
    /// Admitted sources whose policy differs from the target path's policy.
    pub copy_sources: Vec<PathRestrictionCopySourceInfo>,
    /// Declared subtree source, only when its policy differs from target.
    pub subtree_source: Option<SubtreeSourcePolicyInfo>,
}

impl PathPermissionChangeInfo {
    pub fn between(
        base: PathPolicyInfo,
        target: PathPolicyInfo,
        copy_sources: &[PathRestrictionCopySourceInfo],
        subtree_source: Option<SubtreeSourcePolicyInfo>,
    ) -> Option<Self> {
        let copy_sources: Vec<_> = copy_sources
            .iter()
            .filter(|source| source.info != target)
            .cloned()
            .collect();
        let subtree_source = subtree_source.filter(|source| source.info != target);
        if base == target && copy_sources.is_empty() && subtree_source.is_none() {
            None
        } else {
            Some(Self {
                base,
                target,
                copy_sources,
                subtree_source,
            })
        }
    }
}

/// Restriction metadata for a changed file, without content enforcement.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangedPathRestrictionInfo {
    pub sides: ChangedPathRestrictionSides,
    /// Per-file copy sources admitted by the selected parent comparison.
    pub copy_sources: Vec<PathRestrictionCopySourceInfo>,
    /// Policy protecting actual source content, absent if that source has no file.
    pub subtree_source: Option<SubtreeSourcePolicyInfo>,
    /// Sparse policy delta; absent when endpoint and source policies agree.
    /// Opposite policies for absent files do not contribute content restrictions.
    pub permission_change: Option<PathPermissionChangeInfo>,
}

impl ChangedPathRestrictionInfo {
    /// Actual subtree source content, including a source-only deletion.
    pub fn subtree_source(&self) -> Option<&SubtreeSourcePolicyInfo> {
        match &self.sides {
            ChangedPathRestrictionSides::SourceOnly(source) => Some(source),
            _ => self.subtree_source.as_ref(),
        }
    }

    /// Policies protecting content actually present in either endpoint or source.
    pub fn content_sides(&self) -> impl Iterator<Item = &PathPolicyInfo> {
        [self.sides.base(), self.sides.target()]
            .into_iter()
            .flatten()
            .chain(self.copy_sources.iter().map(|source| &source.info))
            .chain(self.subtree_source().map(|source| &source.info))
    }

    /// Restrictions on actual content only, excluding absent-side policies.
    pub fn restriction_info(&self) -> impl Iterator<Item = &PathAccessInfo> {
        self.content_sides().flat_map(|side| &side.restrictions)
    }
}

/// Information about restricted path changes in a changeset.
#[derive(Clone, Debug, PartialEq)]
pub struct RestrictedPathsChangesInfo {
    /// Changed paths that fall under restrictions, grouped by restriction root.
    pub restricted_changes: Vec<RestrictedChangeGroup>,
}

/// A group of changed paths that share the same restriction root.
#[derive(Clone, Debug, PartialEq)]
pub struct RestrictedChangeGroup {
    /// The restriction root and access info covering these changes.
    pub restriction_info: PathAccessInfo,
    // TODO(T248660146): remove this field and `RestrictedChangeGroup` if there's
    // no need to use it for now.
    /// The changed paths under this restriction root.
    pub changed_paths: Vec<NonRootMPath>,
}

/// What to do when restricted-path enforcement denies a path.
///
/// Provided as a parameter to all methods that may encounter denials.
/// Callers that do not handle omissions should use `Strict`.
#[derive(Clone, Debug)]
pub enum RestrictedPathsPolicy {
    /// Denials fail the whole request.
    Strict,
    /// Skip (streams) or omit-or-downgrade (diffs) denied paths, recording
    /// each against its denying restriction in the shared collector.
    /// Repo-auth denials and all other errors still fail the whole request.
    SkipAndCollect(Arc<OmissionCollector>),
}

/// Shared omission state for `SkipAndCollect`: the total omitted count plus
/// per-restriction counts keyed by the denying restriction. Paths are
/// retained per group only when a handler opts in, so unbounded streams
/// never accumulate them.
#[derive(Debug, Default)]
pub struct OmissionCollector {
    omitted: AtomicUsize,
    omitted_entries: AtomicUsize,
    groups: Mutex<BTreeMap<DenyingRestriction, OmissionGroup>>,
    max_omitted: Mutex<Option<MPath>>,
    retain_paths: AtomicBool,
}

/// One group's omission count plus its retained paths, if any.
#[derive(Debug, Default)]
struct OmissionGroup {
    count: usize,
    paths: BTreeSet<MPath>,
}

impl OmissionCollector {
    /// Total omitted paths recorded so far.
    pub fn omitted_count(&self) -> usize {
        self.omitted.load(Ordering::Relaxed)
    }

    /// Total fully-omitted entries noted so far. Unlike the side-based
    /// count, one entry counts once however many sides denied it.
    pub fn omitted_entry_count(&self) -> usize {
        self.omitted_entries.load(Ordering::Relaxed)
    }

    /// Whether omitted paths are being retained (and counted toward page
    /// budgets) for this request.
    pub fn omitted_path_retention(&self) -> bool {
        self.retain_paths.load(Ordering::Relaxed)
    }

    /// Retain omitted paths per group from now on. The constructor turns
    /// this on for requests whose block asks for omitted paths (bounded
    /// unary pages); streams leave it off and emit paths inline instead.
    fn set_path_retention(&self, retain: bool) {
        self.retain_paths.store(retain, Ordering::Relaxed);
    }

    /// Omitted-path counts grouped by denying restriction.
    pub fn omitted_groups(&self) -> BTreeMap<DenyingRestriction, usize> {
        self.groups
            .lock()
            .expect("lock poisoned")
            .iter()
            .map(|(restriction, group)| (restriction.clone(), group.count))
            .collect()
    }

    /// Retained omitted paths grouped by denying restriction, in path
    /// order. Empty unless retention was enabled; manifest denials carry
    /// no path.
    pub fn omitted_paths(&self) -> BTreeMap<DenyingRestriction, Vec<MPath>> {
        self.groups
            .lock()
            .expect("lock poisoned")
            .iter()
            .map(|(restriction, group)| {
                (restriction.clone(), group.paths.iter().cloned().collect())
            })
            .collect()
    }

    /// Record one omitted path, attributed to its denying restriction.
    /// `path` is `None` for manifest denials, which name no path.
    pub fn record(&self, restriction: &DenyingRestriction, path: Option<&MPath>) {
        self.omitted.fetch_add(1, Ordering::Relaxed);
        let mut groups = self.groups.lock().expect("lock poisoned");
        let group = groups.entry(restriction.clone()).or_default();
        group.count += 1;
        if self.retain_paths.load(Ordering::Relaxed)
            && let Some(path) = path
        {
            group.paths.insert(path.clone());
        }
    }

    /// Note one fully-omitted entry. Call downstream of the page take so
    /// lookahead past the budget never counts. `path` feeds the maximum
    /// omitted path; `None` (unparseable) still counts the entry.
    pub fn note_omitted_entry(&self, path: Option<&MPath>) {
        self.omitted_entries.fetch_add(1, Ordering::Relaxed);
        if let Some(path) = path {
            let mut max = self.max_omitted.lock().expect("lock poisoned");
            if max.as_ref().is_none_or(|current| *current < *path) {
                *max = Some(path.clone());
            }
        }
    }

    /// Maximum omitted path noted so far, if any.
    pub fn max_omitted_mpath(&self) -> Option<MPath> {
        self.max_omitted.lock().expect("lock poisoned").clone()
    }
}

/// JustKnobs gate for partial SCS responses. A client gets skipping
/// behavior when the killswitch is on for its repo, or when it sends the
/// opt-in request header. A client that sends the opt-out request header
/// never gets skipping behavior: denials fail the whole request for it,
/// whatever the killswitch says. The killswitch covers SCS path-info,
/// last-changed, file-diffs, compare and find-files, plus the
/// diff-service compare and single-pair diffs behind the remote route.
/// Switched on repo name for per-repo rollout; when off (the default)
/// and no header is sent, denials fail the whole request.
pub const SCS_ENABLE_PARTIAL_RESPONSES_JK: &str = "scm/mononoke:scs_enable_partial_responses";

impl RestrictedPathsPolicy {
    /// Build the policy for the current SCS (or diff-service) request.
    /// Skipping is allowed when the killswitch is on for the repo, the
    /// client opted in via the request header, or the request set
    /// `partial_options`, unless the client opted out: an opted-out
    /// client gets `Strict` even when the killswitch is on. A present
    /// block also turns on path retention when it asks for omitted
    /// paths; `Strict` ignores the block entirely. Evaluate ONCE per
    /// request and share the result across every call, so the knob
    /// decision cannot disagree with itself. A missing knob is an error;
    /// the knob exists with default off, and integration tests enable it
    /// via `merge_just_knobs`.
    pub fn for_scs_request(
        ctx: &CoreContext,
        repo_name: &str,
        partial_options: Option<&PartialOptions>,
    ) -> Self {
        if ctx.metadata().partial_responses_opt_out() {
            return Self::Strict;
        }
        let client_opt_in = ctx.metadata().partial_responses_opt_in();
        let knob_on = justknobs::eval(SCS_ENABLE_PARTIAL_RESPONSES_JK, None, Some(repo_name));
        if client_opt_in || knob_on || partial_options.is_some() {
            let collector = OmissionCollector::default();
            if Self::paths_requested(partial_options) {
                collector.set_path_retention(true);
            }
            Self::SkipAndCollect(Arc::new(collector))
        } else {
            Self::Strict
        }
    }

    /// Whether a partial-options block asks for withheld paths in the
    /// response. An absent block or an unset flag means no paths. The
    /// constructor and the render call sites share this predicate, so
    /// retention and rendering cannot disagree.
    pub fn paths_requested(partial_options: Option<&PartialOptions>) -> bool {
        partial_options.is_some_and(|options| options.include_omitted_paths.unwrap_or(false))
    }

    /// Record one omitted path, attributed to its denying restriction.
    /// `path` is `None` for manifest denials, which name no path.
    pub fn record_denial(&self, restriction: &DenyingRestriction, path: Option<&MPath>) {
        if let Self::SkipAndCollect(collector) = self {
            collector.record(restriction, path);
        }
    }

    /// Whether omitted paths are being retained for this request.
    pub fn omitted_path_retention(&self) -> bool {
        match self {
            Self::Strict => false,
            Self::SkipAndCollect(collector) => collector.omitted_path_retention(),
        }
    }

    /// Note one fully-omitted entry. No-op under `Strict`.
    pub fn note_omitted_entry(&self, path: Option<&MPath>) {
        if let Self::SkipAndCollect(collector) = self {
            collector.note_omitted_entry(path);
        }
    }

    /// Fully-omitted entries noted so far, or `None` when partial
    /// responses are not allowed.
    pub fn omitted_entry_count(&self) -> Option<usize> {
        match self {
            Self::Strict => None,
            Self::SkipAndCollect(collector) => Some(collector.omitted_entry_count()),
        }
    }

    /// Last scanned path of the counted windows: the maximum of
    /// `visible_last` and every noted omitted path, compared as paths.
    /// `None` when both are absent. An unparseable visible path is
    /// ignored, which understates rather than overstates the resume
    /// point.
    pub fn last_scanned_path(&self, visible_last: Option<&str>) -> Option<String> {
        let visible = visible_last.and_then(|s| MPath::new(s).ok());
        let omitted = match self {
            Self::Strict => None,
            Self::SkipAndCollect(collector) => collector.max_omitted_mpath(),
        };
        visible
            .into_iter()
            .chain(omitted)
            .max()
            .map(|path| path.to_string())
    }

    /// Maximum omitted path noted so far, if any.
    pub fn max_omitted_mpath(&self) -> Option<MPath> {
        match self {
            Self::Strict => None,
            Self::SkipAndCollect(collector) => collector.max_omitted_mpath(),
        }
    }

    /// Number of paths omitted so far, or `None` when partial responses
    /// are not allowed.
    pub fn omitted_count(&self) -> Option<usize> {
        match self {
            Self::Strict => None,
            Self::SkipAndCollect(collector) => Some(collector.omitted_count()),
        }
    }

    /// Omitted-path counts grouped by denying restriction, or `None` when
    /// partial responses are not allowed.
    pub fn omitted_groups(&self) -> Option<BTreeMap<DenyingRestriction, usize>> {
        match self {
            Self::Strict => None,
            Self::SkipAndCollect(collector) => Some(collector.omitted_groups()),
        }
    }

    /// Retained omitted paths grouped by denying restriction, or `None`
    /// when partial responses are not allowed.
    pub fn omitted_paths(&self) -> Option<BTreeMap<DenyingRestriction, Vec<MPath>>> {
        match self {
            Self::Strict => None,
            Self::SkipAndCollect(collector) => Some(collector.omitted_paths()),
        }
    }

    /// Set the partial-response flag when paths were omitted.
    pub fn set_partial_if_omitted(&self, ctx: &CoreContext) {
        if self.omitted_count().is_some_and(|count| count > 0) {
            ctx.set_partial_response();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use fbinit::FacebookInit;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use metadata::Metadata;
    use mononoke_macros::mononoke;
    use permission_checker::MononokeIdentity;

    use super::*;

    // What it tests: equality, sorting and grouping use one complete checked identity.
    // Expected: only identical rows coalesce; every policy/access difference remains.
    #[mononoke::test]
    fn checked_restriction_order_matches_equality_and_grouping() -> Result<(), anyhow::Error> {
        let original = PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("secret")?,
                repo_region_acl: "REPO_REGION:a".into(),
                permission_request_group: MononokeIdentity::from_str("GROUP:a")?,
                rollout_allowlist_group: None,
            },
            has_access: Some(false),
        };
        let mut variants = vec![original.clone()];
        let mut changed = original.clone();
        changed.restriction.restriction_root = NonRootMPath::new("other")?;
        variants.push(changed);
        let mut changed = original.clone();
        changed.restriction.repo_region_acl = "REPO_REGION:b".into();
        variants.push(changed);
        let mut changed = original.clone();
        changed.restriction.permission_request_group = MononokeIdentity::from_str("GROUP:b")?;
        variants.push(changed);
        let mut changed = original.clone();
        changed.restriction.rollout_allowlist_group =
            Some(MononokeIdentity::from_str("GROUP:rollout")?);
        variants.push(changed);
        let mut changed = original.clone();
        changed.has_access = None;
        variants.push(changed);
        let mut changed = original.clone();
        changed.has_access = Some(true);
        variants.push(changed);
        variants.push(original);
        for left in &variants {
            for right in &variants {
                assert_eq!(left.cmp(right).is_eq(), left == right);
            }
        }
        let mut sorted = variants.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted[0].restriction_root().to_string(), "other");
        let grouped: BTreeMap<_, usize> =
            variants
                .into_iter()
                .fold(BTreeMap::new(), |mut groups, info| {
                    *groups.entry(info).or_default() += 1;
                    groups
                });
        assert_eq!(sorted, grouped.keys().cloned().collect::<Vec<_>>());
        assert_eq!(sorted.len(), 7);
        assert_eq!(grouped.values().filter(|count| **count == 2).count(), 1);
        Ok(())
    }

    fn knobs(enabled: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(HashMap::from([(
            SCS_ENABLE_PARTIAL_RESPONSES_JK.to_string(),
            KnobVal::Bool(enabled),
        )]))
    }

    fn ctx_with_opt_flags(fb: FacebookInit, opt_in: bool, opt_out: bool) -> CoreContext {
        let mut metadata = Metadata::default();
        metadata.add_partial_responses_opt_in(opt_in);
        metadata.add_partial_responses_opt_out(opt_out);
        CoreContext::test_mock(fb).with_overridden_metadata(Arc::new(metadata))
    }

    #[mononoke::fbinit_test]
    async fn strict_without_knob_or_opt_in(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, false, false);
        with_just_knobs_async(
            knobs(false),
            async move {
                assert!(matches!(
                    RestrictedPathsPolicy::for_scs_request(&ctx, "repo", None),
                    RestrictedPathsPolicy::Strict
                ));
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn opt_in_skips_without_knob(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, true, false);
        with_just_knobs_async(
            knobs(false),
            async move {
                let policy = RestrictedPathsPolicy::for_scs_request(&ctx, "repo", None);
                assert_eq!(policy.omitted_count(), Some(0));
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn knob_skips_without_opt_in(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, false, false);
        with_just_knobs_async(
            knobs(true),
            async move {
                let policy = RestrictedPathsPolicy::for_scs_request(&ctx, "repo", None);
                assert_eq!(policy.omitted_count(), Some(0));
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn opt_out_is_strict_despite_knob(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, false, true);
        with_just_knobs_async(
            knobs(true),
            async move {
                assert!(matches!(
                    RestrictedPathsPolicy::for_scs_request(&ctx, "repo", None),
                    RestrictedPathsPolicy::Strict
                ));
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn opt_out_beats_opt_in(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, true, true);
        with_just_knobs_async(
            knobs(false),
            async move {
                assert!(matches!(
                    RestrictedPathsPolicy::for_scs_request(&ctx, "repo", None),
                    RestrictedPathsPolicy::Strict
                ));
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn block_opts_in_without_knob(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, false, false);
        let block = PartialOptions {
            include_omitted_paths: Some(true),
            ..Default::default()
        };
        with_just_knobs_async(
            knobs(false),
            async move {
                let policy = RestrictedPathsPolicy::for_scs_request(&ctx, "repo", Some(&block));
                assert_eq!(policy.omitted_count(), Some(0));
                // The block's flag turns on retention at construction.
                let restriction = denying_restriction("tent", "REPO_REGION:tent_acl")?;
                policy.record_denial(&restriction, Some(&MPath::new("tent/secret")?));
                assert_eq!(
                    policy.omitted_paths(),
                    Some(BTreeMap::from([(
                        restriction,
                        vec![MPath::new("tent/secret")?]
                    )])),
                );
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn block_without_flag_skips_without_retention(
        fb: FacebookInit,
    ) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, false, false);
        let block = PartialOptions {
            include_omitted_paths: None,
            ..Default::default()
        };
        with_just_knobs_async(
            knobs(false),
            async move {
                let policy = RestrictedPathsPolicy::for_scs_request(&ctx, "repo", Some(&block));
                assert_eq!(policy.omitted_count(), Some(0));
                // Presence alone skips; without the flag, paths are dropped.
                let restriction = denying_restriction("tent", "REPO_REGION:tent_acl")?;
                policy.record_denial(&restriction, Some(&MPath::new("tent/secret")?));
                assert_eq!(policy.omitted_count(), Some(1));
                assert!(
                    policy
                        .omitted_paths()
                        .is_some_and(|paths| paths.values().all(|group| group.is_empty()))
                );
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn opt_out_beats_block(fb: FacebookInit) -> Result<(), anyhow::Error> {
        let ctx = ctx_with_opt_flags(fb, false, true);
        let block = PartialOptions {
            include_omitted_paths: Some(true),
            ..Default::default()
        };
        with_just_knobs_async(
            knobs(false),
            async move {
                assert!(matches!(
                    RestrictedPathsPolicy::for_scs_request(&ctx, "repo", Some(&block)),
                    RestrictedPathsPolicy::Strict
                ));
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    #[mononoke::test]
    fn collect_attributes_counts_by_restriction() -> Result<(), anyhow::Error> {
        let collector = OmissionCollector::default();
        let tent_a = denying_restriction("tent_a", "REPO_REGION:a_acl")?;
        let tent_b = denying_restriction("tent_b", "REPO_REGION:b_acl")?;
        collector.record(&tent_a, Some(&MPath::new("tent_a/secret")?));
        collector.record(&tent_b, None);
        collector.record(&tent_a, Some(&MPath::new("tent_a/other")?));

        assert_eq!(collector.omitted_count(), 3);
        assert_eq!(
            collector.omitted_groups(),
            BTreeMap::from([(tent_a, 2), (tent_b, 1)]),
        );
        Ok(())
    }

    #[mononoke::test]
    fn collect_retains_paths_only_when_enabled() -> Result<(), anyhow::Error> {
        // Retention off: paths are dropped, counts still recorded.
        let collector = OmissionCollector::default();
        let tent = denying_restriction("tent", "REPO_REGION:tent_acl")?;
        collector.record(&tent, Some(&MPath::new("tent/b")?));
        collector.record(&tent, Some(&MPath::new("tent/a")?));
        collector.record(&tent, Some(&MPath::new("tent/a")?));
        assert_eq!(collector.omitted_count(), 3);
        assert!(
            collector
                .omitted_paths()
                .values()
                .all(|paths| paths.is_empty())
        );

        // Retention on: paths are kept sorted and deduplicated per group.
        // A path recorded twice still counts twice.
        collector.set_path_retention(true);
        collector.record(&tent, Some(&MPath::new("tent/b")?));
        collector.record(&tent, Some(&MPath::new("tent/a")?));
        collector.record(&tent, Some(&MPath::new("tent/a")?));
        assert_eq!(collector.omitted_count(), 6);
        assert_eq!(
            collector.omitted_paths(),
            BTreeMap::from([(tent, vec![MPath::new("tent/a")?, MPath::new("tent/b")?])]),
        );
        Ok(())
    }

    #[mononoke::test]
    fn policy_collects_groups_and_ignores_strict() -> Result<(), anyhow::Error> {
        let policy = RestrictedPathsPolicy::SkipAndCollect(Arc::new(OmissionCollector::default()));
        let restriction = denying_restriction("tent", "REPO_REGION:tent_acl")?;
        policy.record_denial(&restriction, Some(&MPath::new("tent/secret")?));
        policy.record_denial(&restriction, None);

        assert_eq!(policy.omitted_count(), Some(2));
        assert_eq!(
            policy.omitted_groups(),
            Some(BTreeMap::from([(restriction, 2)])),
        );

        let strict = RestrictedPathsPolicy::Strict;
        strict.record_denial(&denying_restriction("tent", "REPO_REGION:tent_acl")?, None);
        assert_eq!(strict.omitted_count(), None);
        assert_eq!(strict.omitted_groups(), None);
        assert_eq!(strict.omitted_paths(), None);
        Ok(())
    }

    #[mononoke::test]
    fn noted_entries_track_count_and_max_omitted() -> Result<(), anyhow::Error> {
        let collector = OmissionCollector::default();
        assert_eq!(collector.omitted_entry_count(), 0);
        assert_eq!(collector.max_omitted_mpath(), None);

        let first = MPath::new("tent/a")?;
        let second = MPath::new("tent/b")?;
        collector.note_omitted_entry(Some(&first));
        // A pathless note still counts the entry.
        collector.note_omitted_entry(None);
        collector.note_omitted_entry(Some(&second));
        // Noting out of order keeps the maximum.
        collector.note_omitted_entry(Some(&first));

        assert_eq!(collector.omitted_entry_count(), 4);
        assert_eq!(collector.max_omitted_mpath(), Some(second));
        Ok(())
    }

    #[mononoke::test]
    fn last_scanned_path_maxes_visible_and_omitted() -> Result<(), anyhow::Error> {
        let policy = RestrictedPathsPolicy::SkipAndCollect(Arc::new(OmissionCollector::default()));
        // Neither side noted: no continuation.
        assert_eq!(policy.last_scanned_path(None), None);

        policy.note_omitted_entry(Some(&MPath::new("tent/b")?));
        // Omitted above visible wins ...
        assert_eq!(
            policy.last_scanned_path(Some("tent/a")),
            Some("tent/b".to_string())
        );
        // ... and visible above omitted wins.
        assert_eq!(
            policy.last_scanned_path(Some("tent/c")),
            Some("tent/c".to_string())
        );
        // An unparseable visible path is ignored, understating the resume.
        assert_eq!(
            policy.last_scanned_path(Some("tent/\u{0}")),
            Some("tent/b".to_string())
        );

        // Comparison is in path space, not string space: component-wise
        // "a-b" sorts after "a/b", while strings order them the other way.
        let trap = RestrictedPathsPolicy::SkipAndCollect(Arc::new(OmissionCollector::default()));
        trap.note_omitted_entry(Some(&MPath::new("a/b")?));
        assert_eq!(trap.last_scanned_path(Some("a-b")), Some("a-b".to_string()));
        Ok(())
    }

    #[mononoke::test]
    fn strict_ignores_entry_noting() -> Result<(), anyhow::Error> {
        let strict = RestrictedPathsPolicy::Strict;
        strict.note_omitted_entry(Some(&MPath::new("tent/a")?));
        assert_eq!(strict.omitted_entry_count(), None);
        assert_eq!(strict.max_omitted_mpath(), None);
        assert_eq!(
            strict.last_scanned_path(Some("tent/a")),
            Some("tent/a".to_string())
        );
        assert_eq!(strict.last_scanned_path(None), None);
        Ok(())
    }

    fn denying_restriction(root: &str, acl: &str) -> Result<DenyingRestriction, anyhow::Error> {
        let acl = MononokeIdentity::from_str(acl)?;
        Ok(DenyingRestriction {
            restriction_root: Some(NonRootMPath::new(root)?),
            repo_region_acl: acl.clone(),
            permission_request_group: acl,
        })
    }
}
