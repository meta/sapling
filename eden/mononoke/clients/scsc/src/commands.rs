/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::ScscApp;

base_app::subcommands! {
    type App = ScscApp;
    mod cat;
    mod blame;
    mod commit_fingerprint;
    mod commit_rate_limit_check;
    mod common_base;
    mod create_git_bundle;
    mod create_repos if "SCSC_ADMIN_ENABLED";
    mod create_bookmark if "SCSC_WRITES_ENABLED";
    mod delete_bookmark if "SCSC_WRITES_ENABLED";
    mod diff;
    mod directory_branch_clusters;
    mod file_diff;
    mod export;
    mod filter_ancestors;
    mod find_files;
    mod info;
    mod is_ancestor;
    mod land_stack if "SCSC_WRITES_ENABLED";
    mod last_changed;
    mod list_bookmarks;
    mod log;
    mod lookup;
    mod ls;
    mod move_bookmark if "SCSC_WRITES_ENABLED";
    mod path_first_commit;
    mod prepare_commits if "SCSC_WRITES_ENABLED";
    mod rebase_stack if "SCSC_WRITES_ENABLED";
    mod pushrebase_history;
    mod restricted_paths;
    mod git_mutation_history;
    mod hg_mutation_history;
    mod repo_exists;
    mod repo_info;
    mod repos;
    mod run_hooks;
    #[cfg(all(target_os = "linux", fbcode_build))]
    mod scmqueryclient_test if "SCSC_SCMQUERY_TEST_ENABLED";
    mod sparse_profile_delta;
    mod sparse_profile_size;
    mod update_submodule_expansion;
    mod xrepo_lookup;
}
