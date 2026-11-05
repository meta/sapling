/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;

use faster_hex::hex_string;
use itertools::Itertools;
use scuba_ext::MononokeScubaSampleBuilder;
use scuba_ext::ScubaValue;
use source_control as thrift;

/// To avoid logging very large numbers of commit ids to scuba, we limit
/// requests that potentially involve unbounded numbers of commits to logging
/// just the first few.
const COMMIT_LIMIT: usize = 10;

/// A trait for logging a thrift `Params` struct to scuba.
///
/// Common and meaningful parameters (e.g. `other_commit` or
/// `identity_schemes` are logged using that name.  Other parameters
/// should have their name prefixed by `param_` to make it clear that
/// the column is for a parameter.  The default implementation does
/// nothing.
pub(crate) trait AddScubaParams: Send + Sync {
    fn add_scuba_params(&self, _scuba: &mut MononokeScubaSampleBuilder) {}
}

impl AddScubaParams for BTreeSet<thrift::CommitIdentityScheme> {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "identity_schemes",
            self.iter().map(ToString::to_string).collect::<ScubaValue>(),
        );
    }
}

impl AddScubaParams for thrift::ListReposParams {}

impl AddScubaParams for thrift::RepoExistsParams {}
impl AddScubaParams for thrift::StorageConfigExistsParams {}
impl AddScubaParams for thrift::CreateOrgStorageConfigParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("org_slug", self.org.clone());
        scuba.add("manifold_bucket", self.manifold_bucket.clone());
        scuba.add("xdb_shard", self.xdb_shard.clone());
    }
}
impl AddScubaParams for thrift::DeleteOrgStorageConfigParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("org_slug", self.org.clone());
    }
}
impl AddScubaParams for thrift::OrgStorageConfigPollParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("mutation_id", self.mutation_id);
    }
}
impl AddScubaParams for thrift::OrgStorageConfigParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("org_slug", self.org.clone());
    }
}

impl AddScubaParams for thrift::RepoCreateCommitParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_parents",
            self.parents
                .iter()
                .map(ToString::to_string)
                .collect::<ScubaValue>(),
        );
        if let Some(date) = self.info.date.as_ref() {
            scuba.add("param_date", date.timestamp);
        }
        scuba.add("param_author", self.info.author.as_str());
        let deletes_count = self
            .changes
            .values()
            .filter(|change| matches!(change, thrift::RepoCreateCommitParamsChange::deleted(_)))
            .count();
        scuba.add("param_changes_count", self.changes.len() - deletes_count);
        scuba.add("param_deletes_count", deletes_count);
        self.identity_schemes.add_scuba_params(scuba);
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
        if let Some(mutation_info) = self.mutation_info.as_ref() {
            scuba.add("param_mutation_op", mutation_info.op.as_str());
            if let Some(user) = mutation_info.user.as_deref() {
                scuba.add("param_mutation_user", user);
            }
            scuba.add(
                "param_mutation_predecessor_count",
                mutation_info.predecessors.len(),
            );
        }
    }
}

impl AddScubaParams for thrift::RepoFoldCommitsParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_bottom", self.bottom.to_string());
        if let Some(top) = &self.top {
            scuba.add("param_top", top.to_string());
        }
        if let Some(info) = self.info.as_ref() {
            if let Some(date) = info.date.as_ref() {
                scuba.add("param_date", date.timestamp);
            }
            scuba.add("param_author", info.author.as_str());
        }
        let deletes_count = self
            .changes
            .values()
            .filter(|change| matches!(change, thrift::RepoCreateCommitParamsChange::deleted(_)))
            .count();
        scuba.add("param_changes_count", self.changes.len() - deletes_count);
        scuba.add("param_deletes_count", deletes_count);
        self.identity_schemes.add_scuba_params(scuba);
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
        if let Some(mutation_info) = self.mutation_info.as_ref() {
            scuba.add("param_mutation_op", mutation_info.op.as_str());
            scuba.add("param_mutation_user", mutation_info.user.as_str());
        }
        if self.keep_folded_commits {
            scuba.add("param_keep_folded_commits", true);
        }
    }
}

impl AddScubaParams for thrift::RepoCreateStackParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_parents",
            self.parents
                .iter()
                .map(ToString::to_string)
                .collect::<ScubaValue>(),
        );
        let deletes_count = self
            .commits
            .iter()
            .map(|commit| {
                commit
                    .changes
                    .values()
                    .filter(|change| {
                        matches!(change, thrift::RepoCreateCommitParamsChange::deleted(_))
                    })
                    .count()
            })
            .sum::<usize>();
        let changes_count = self
            .commits
            .iter()
            .map(|commit| commit.changes.len())
            .sum::<usize>();
        scuba.add("param_changes_count", changes_count - deletes_count);
        scuba.add("param_deletes_count", deletes_count);
        scuba.add("param_commit_count", self.commits.len());
        self.identity_schemes.add_scuba_params(scuba);
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
        let mutation_ops = self
            .commits
            .iter()
            .filter_map(|commit| commit.mutation_info.as_ref())
            .map(|mutation_info| mutation_info.op.clone())
            .collect::<BTreeSet<_>>();
        if !mutation_ops.is_empty() {
            scuba.add(
                "param_mutation_ops",
                mutation_ops.into_iter().collect::<ScubaValue>(),
            );
        }
    }
}

impl AddScubaParams for thrift::RepoCreateBookmarkParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark.as_str());
        scuba.add("commit", self.target.to_string());
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
    }
}

impl AddScubaParams for thrift::RepoMoveBookmarkParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark.as_str());
        scuba.add("commit", self.target.to_string());
        if let Some(old_target) = &self.old_target {
            scuba.add("param_old_target", old_target.to_string());
        }
        scuba.add(
            "param_allow_non_fast_forward_move",
            self.allow_non_fast_forward_move as i32,
        );
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
    }
}

impl AddScubaParams for thrift::RepoDeleteBookmarkParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark.as_str());
        if let Some(old_target) = &self.old_target {
            scuba.add("param_old_target", old_target.to_string());
        }
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
    }
}

impl AddScubaParams for thrift::RepoLandStackParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark.as_str());
        scuba.add("commit", self.head.to_string());
        scuba.add("param_base", self.base.to_string());
        self.identity_schemes.add_scuba_params(scuba);
        if let Some(old_identity_schemes) = &self.old_identity_schemes {
            scuba.add(
                "old_identity_schemes",
                old_identity_schemes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<ScubaValue>(),
            );
        }
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
    }
}

impl AddScubaParams for thrift::RepoRebaseStackParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("commit", self.head.to_string());
        scuba.add("param_base", self.base.to_string());
        scuba.add("param_onto", self.onto.to_string());
        scuba.add("merge_resolution", self.merge_resolution.to_string());
        self.identity_schemes.add_scuba_params(scuba);
        if let Some(old_identity_schemes) = &self.old_identity_schemes {
            scuba.add(
                "old_identity_schemes",
                old_identity_schemes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<ScubaValue>(),
            );
        }
        if let Some(service_identity) = self.service_identity.as_deref() {
            scuba.add("service_identity", service_identity);
        }
    }
}

impl AddScubaParams for thrift::RepoListBookmarksParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_include_scratch", self.include_scratch as i32);
        scuba.add("param_bookmark_prefix", self.bookmark_prefix.as_str());
        scuba.add("param_limit", self.limit);
        if let Some(after) = &self.after {
            scuba.add("param_after", after.as_str());
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoResolveBookmarkParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark_name.as_str());
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoResolveP4FileRevisionParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_path", self.path.as_str());
        scuba.add("param_revision", self.revision);
        if let Some(bookmark) = &self.bookmark {
            scuba.add("param_bookmark", bookmark.as_str());
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoBookmarkInfoParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark_name.as_str());
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoResolveCommitPrefixParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_prefix", self.prefix.as_str());
        scuba.add("param_prefix_scheme", self.prefix_scheme.to_string());
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoStackInfoParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_first_n_commits",
            self.heads
                .iter()
                .take(COMMIT_LIMIT)
                .map(ToString::to_string)
                .collect::<ScubaValue>(),
        );
        scuba.add("param_commit_count", self.heads.len());
        scuba.add("param_limit", self.limit);
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoPrepareCommitsParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_first_n_commits",
            self.commits
                .iter()
                .take(COMMIT_LIMIT)
                .map(ToString::to_string)
                .collect::<ScubaValue>(),
        );
        scuba.add("param_commit_count", self.commits.len());
        scuba.add(
            "param_derived_data_type",
            self.derived_data_type.to_string(),
        );
    }
}

impl AddScubaParams for thrift::RepoUploadFileContentParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_data_len", self.data.len());
    }
}

impl AddScubaParams for thrift::CommitCompareParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        if let Some(other_commit_id) = self.other_commit_id.as_ref() {
            scuba.add("other_commit", other_commit_id.to_string());
        }
        if let Some(paths) = &self.paths {
            scuba.add("param_paths", paths.iter().collect::<ScubaValue>());
        }
        if let Some(ordered_params) = &self.ordered_params {
            if let Some(after_path) = &ordered_params.after_path {
                scuba.add("param_after", after_path.as_str());
            }
            scuba.add("param_limit", ordered_params.limit);
        }
        scuba.add("param_skip_copies_renames", self.skip_copies_renames as i32);
        if let Some(compare_with_subtree_copy_sources) = self.compare_with_subtree_copy_sources {
            scuba.add(
                "param_compare_with_subtree_copy_sources",
                compare_with_subtree_copy_sources as i32,
            );
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitFileDiffsParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "other_commit",
            self.other_commit_id.as_ref().map(|oci| oci.to_string()),
        );
        scuba.add("param_format", self.format.to_string());
        scuba.add("param_context", self.context);
    }
}

impl AddScubaParams for thrift::CommitFindFilesParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_limit", self.limit);
        if let Some(basenames) = &self.basenames {
            scuba.add("param_basenames", basenames.iter().collect::<ScubaValue>());
        }
        if let Some(suffixes) = &self.basename_suffixes {
            scuba.add("param_suffixes", suffixes.iter().collect::<ScubaValue>());
        }
        if let Some(prefixes) = &self.prefixes {
            scuba.add("param_prefixes", prefixes.iter().collect::<ScubaValue>());
        }
        if let Some(after) = &self.after {
            scuba.add("param_after", after.as_str());
        }
    }
}

impl AddScubaParams for thrift::CommitInfoParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitGenerationParams {}

impl AddScubaParams for thrift::CommitFingerprintParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("fingerprint_version", format!("{:?}", self.version));
    }
}

impl AddScubaParams for thrift::CommitIsAncestorOfParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("other_commit", self.descendant_commit_id.to_string());
    }
}

impl AddScubaParams for thrift::CommitFilterAncestorsParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_num_candidates", self.candidate_ancestor_ids.len());
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitIsPublicParams {}

impl AddScubaParams for thrift::CommitCommonBaseWithParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("other_commit", self.other_commit_id.to_string());
    }
}

impl AddScubaParams for thrift::CommitLookupParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::RepoMultipleCommitLookupParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitLookupPushrebaseHistoryParams {}

impl AddScubaParams for thrift::CommitHistoryParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_format", self.format.to_string());
        scuba.add("param_skip", self.skip);
        scuba.add("param_limit", self.limit);
        if let Some(before) = self.before_timestamp {
            scuba.add("param_before_timestamp", before);
        }
        if let Some(after) = self.after_timestamp {
            scuba.add("param_after_timestamp", after);
        }
        if let Some(descendants_of) = &self.descendants_of {
            scuba.add("param_descendants_of", descendants_of.to_string());
        }
        if let Some(exclude_changeset_and_ancestors) = &self.exclude_changeset_and_ancestors {
            scuba.add(
                "param_exclude_changeset_and_ancestors",
                exclude_changeset_and_ancestors.to_string(),
            );
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitLinearHistoryParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_format", self.format.to_string());
        scuba.add("param_skip", self.skip);
        scuba.add("param_limit", self.limit);
        if let Some(descendants_of) = &self.descendants_of {
            scuba.add("param_descendants_of", descendants_of.to_string());
        }
        if let Some(exclude_changeset_and_ancestors) = &self.exclude_changeset_and_ancestors {
            scuba.add(
                "param_exclude_changeset_and_ancestors",
                exclude_changeset_and_ancestors.to_string(),
            );
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitListDescendantBookmarksParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_include_scratch", self.include_scratch as i32);
        scuba.add("param_bookmark_prefix", self.bookmark_prefix.as_str());
        scuba.add("param_limit", self.limit);
        if let Some(after) = &self.after {
            scuba.add("param_after", after.as_str());
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitRunHooksParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark.as_str());
    }
}

impl AddScubaParams for thrift::CommitRateLimitCheckParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("bookmark_name", self.bookmark.as_str());
    }
}

impl AddScubaParams for thrift::CommitSubtreeChangesParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitLookupXRepoParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("other_repo", self.other_repo.name.as_str());
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitPathBlameParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_format", self.format.to_string());
        if let Some(param_blame_format_option) = &self.format_options {
            let repr = param_blame_format_option
                .iter()
                .map(|x| format!("{x}"))
                .join("|");
            scuba.add("param_format_options", repr);
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitPathHistoryParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_format", self.format.to_string());
        scuba.add("param_skip", self.skip);
        scuba.add("param_limit", self.limit);
        if let Some(before) = self.before_timestamp {
            scuba.add("param_before_timestamp", before);
        }
        if let Some(after) = self.after_timestamp {
            scuba.add("param_after_timestamp", after);
        }
        scuba.add(
            "follow_history_across_deletions",
            self.follow_history_across_deletions,
        );
        if let Some(descendants_of) = &self.descendants_of {
            scuba.add("param_descendants_of", descendants_of.to_string());
        }
        if let Some(exclude_changeset_and_ancestors) = &self.exclude_changeset_and_ancestors {
            scuba.add(
                "param_exclude_changeset_and_ancestors",
                exclude_changeset_and_ancestors.to_string(),
            );
        }
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitHgMutationHistoryParams {}

impl AddScubaParams for thrift::CommitGitMutationHistoryParams {}

impl AddScubaParams for thrift::CommitDirectoryBranchClustersParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        if let Some(paths) = &self.paths {
            scuba.add("param_paths", paths.iter().collect::<ScubaValue>());
        }
        if let Some(after_path) = &self.after_path {
            scuba.add("param_after_path", after_path.as_str());
        }
        scuba.add("param_limit", self.limit);
    }
}

impl AddScubaParams for thrift::CommitChangedPathsApproxParams {}

impl AddScubaParams for thrift::CommitPathExistsParams {}

impl AddScubaParams for thrift::CommitPathInfoParams {}

impl AddScubaParams for thrift::CommitRestrictedPathsAccessParams {}
impl AddScubaParams for thrift::CommitFindRestrictedPathsParams {}
impl AddScubaParams for thrift::CommitRestrictedPathsChangesParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        if let Some(other_commit_id) = self.other_commit_id.as_ref() {
            scuba.add("other_commit", other_commit_id.to_string());
        }
        if let Some(format) = self.format {
            scuba.add("param_restriction_format", format.to_string());
        }
        if let Some(ordered) = &self.ordered_params {
            scuba.add("param_limit", ordered.limit);
            if let Some(after_path) = &ordered.after_path {
                scuba.add("param_after_path", after_path.as_str());
            }
        }
    }
}

impl AddScubaParams for thrift::RepoInfoParams {}

impl AddScubaParams for thrift::GitRepoStateParams {}

impl AddScubaParams for thrift::CommitMultiplePathInfoParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_paths", self.paths.iter().collect::<ScubaValue>());
    }
}

impl AddScubaParams for thrift::CommitPathLastChangedParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        self.identity_schemes.add_scuba_params(scuba);
        scuba.add("param_include_p4_revision", self.include_p4_revision);
    }
}

impl AddScubaParams for thrift::CommitPathFirstChangedParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        self.identity_schemes.add_scuba_params(scuba);
    }
}

impl AddScubaParams for thrift::CommitMultiplePathLastChangedParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_paths", self.paths.iter().collect::<ScubaValue>());
        self.identity_schemes.add_scuba_params(scuba);
        scuba.add("param_include_p4_revision", self.include_p4_revision);
    }
}

impl AddScubaParams for thrift::CommitSparseProfileDeltaParamsV2 {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo", self.commit.repo.to_string());
        scuba.add("commit", self.commit.id.to_string());
        scuba.add("other_commit", self.other_id.to_string());
        scuba.add(
            "param_paths",
            match &self.profiles {
                thrift::SparseProfiles::all_profiles(_) => {
                    ScubaValue::from("all sparse profiles".to_string())
                }
                thrift::SparseProfiles::profiles(profiles) => {
                    profiles.iter().collect::<ScubaValue>()
                }
                thrift::SparseProfiles::UnknownField(t) => {
                    ScubaValue::from(format!("unknown SparseProfiles type {t}"))
                }
            },
        );
    }
}

impl AddScubaParams for thrift::CommitSparseProfileDeltaToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("token", self.id.to_string());
    }
}

impl AddScubaParams for thrift::CommitSparseProfileSizeParamsV2 {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo", self.commit.repo.to_string());
        scuba.add("commit", self.commit.id.to_string());
        scuba.add(
            "param_paths",
            match &self.profiles {
                thrift::SparseProfiles::all_profiles(_) => {
                    ScubaValue::from("all sparse profiles".to_string())
                }
                thrift::SparseProfiles::profiles(profiles) => {
                    profiles.iter().collect::<ScubaValue>()
                }
                thrift::SparseProfiles::UnknownField(t) => {
                    ScubaValue::from(format!("unknown SparseProfiles type {t}"))
                }
            },
        );
    }
}

impl AddScubaParams for thrift::CommitSparseProfileSizeToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("token", self.id.to_string());
    }
}

impl AddScubaParams for thrift::FileContentChunkParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_offset", self.offset);
        scuba.add("param_size", self.size);
    }
}

impl AddScubaParams for thrift::FileExistsParams {}

impl AddScubaParams for thrift::FileInfoParams {}

impl AddScubaParams for thrift::FileDiffParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("other_file", hex_string(&self.other_file_id));
        scuba.add("param_format", self.format.to_string());
        scuba.add("param_context", self.context);
    }
}

impl AddScubaParams for thrift::TreeExistsParams {}

impl AddScubaParams for thrift::TreeListParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_offset", self.offset);
        scuba.add("param_limit", self.limit);
    }
}

impl AddScubaParams for thrift::CreateReposParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_repos",
            self.repos
                .iter()
                .map(|repo| repo.repo_name.clone())
                .collect_vec(),
        );
    }
}

impl AddScubaParams for thrift::CreateReposToken {}

impl AddScubaParams for thrift::MegarepoAddTargetToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_token", self.id);
    }
}

impl AddScubaParams for thrift::MegarepoAddBranchingTargetToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_token", self.id);
    }
}

impl AddScubaParams for thrift::MegarepoChangeConfigToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_token", self.id);
    }
}

impl AddScubaParams for thrift::MegarepoRemergeSourceToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_token", self.id);
    }
}

impl AddScubaParams for thrift::MegarepoSyncChangesetToken {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_token", self.id);
    }
}

impl AddScubaParams for thrift::MegarepoSyncChangesetParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_source_name", self.source_name.clone());
        scuba.add("param_megarepo_cs_id", hex_string(&self.cs_id));
        report_megarepo_target(&self.target, scuba);
    }
}

impl AddScubaParams for thrift::MegarepoRemergeSourceParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_source_name", self.source_name.clone());
        scuba.add("param_megarepo_cs_id", hex_string(&self.cs_id));
        scuba.add(
            "param_megarepo_target_location",
            hex_string(&self.target_location),
        );
        scuba.add("param_megarepo_message", self.message.clone());
        report_megarepo_target(&self.target, scuba);
    }
}

impl AddScubaParams for thrift::MegarepoChangeTargetConfigParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_version", self.new_version.clone());
        scuba.add("param_megarepo_message", self.message.clone());
        scuba.add(
            "param_megarepo_target_location",
            hex_string(&self.target_location),
        );
        report_megarepo_target(&self.target, scuba);
    }
}

impl AddScubaParams for thrift::MegarepoAddTargetParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_megarepo_version",
            self.config_with_new_target.version.clone(),
        );
        scuba.add("param_megarepo_message", self.message.clone());
        report_megarepo_target(&self.config_with_new_target.target, scuba);
    }
}

impl AddScubaParams for thrift::MegarepoAddBranchingTargetParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_megarepo_branching_point",
            hex_string(&self.branching_point),
        );
        report_megarepo_target(&self.target, scuba);
    }
}

impl AddScubaParams for thrift::MegarepoAddConfigParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_megarepo_version", self.new_config.version.clone());
        report_megarepo_target(&self.new_config.target, scuba);
    }
}

impl AddScubaParams for thrift::MegarepoReadConfigParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        report_megarepo_target(&self.target, scuba);
    }
}

// TODO(T179531912): Log params to scuba
impl AddScubaParams for thrift::RepoUpdateSubmoduleExpansionParams {}

impl AddScubaParams for thrift::RepoUploadNonBlobGitObjectParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_git_object_id", hex_string(&self.git_hash));
    }
}

impl AddScubaParams for thrift::CreateGitTreeParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_git_object_id", hex_string(&self.git_tree_hash));
    }
}

impl AddScubaParams for thrift::CreateGitTagParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "param_tagged_changeset_id",
            hex_string(&self.target_changeset),
        );
    }
}

impl AddScubaParams for thrift::RepoStackGitBundleStoreParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_head", self.head.to_string());
        scuba.add("param_base", self.base.to_string());
    }
}

impl AddScubaParams for thrift::RepoUploadPackfileBaseItemParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_git_object_id", hex_string(&self.git_hash));
    }
}

impl AddScubaParams for thrift::CloudWorkspaceInfoParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_workspace", self.workspace.name.clone());
    }
}

impl AddScubaParams for thrift::CloudUserWorkspacesParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_user", self.user.clone());
    }
}

impl AddScubaParams for thrift::CloudWorkspaceSmartlogParams {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_workspace", self.workspace.name.clone());
    }
}

impl AddScubaParams for thrift::RepoTagInfoRequest {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("param_tag_count", self.tag_to_hash_map.len());
    }
}

pub(crate) fn report_megarepo_target(
    target: &thrift::MegarepoTarget,
    scuba: &mut MononokeScubaSampleBuilder,
) {
    scuba.add("param_megarepo_target_bookmark", target.bookmark.clone());
    scuba.add("param_megarepo_target_repo_id", target.repo_id);
}
