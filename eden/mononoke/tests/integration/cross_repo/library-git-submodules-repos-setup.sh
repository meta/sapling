#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Store common functions related to setting up repos for cross-repo sync
# with git submodules integration tests.
# Functions here create/imports repos, add/modify commits or merge them.

# shellcheck source=fbcode/eden/mononoke/tests/integration/library.sh
. "${TEST_FIXTURES}/library.sh"
. "${TEST_FIXTURES}/library-push-redirector.sh"
. "${TEST_FIXTURES}/cross_repo/library-git-submodules-config-setup.sh"
. "${TEST_FIXTURES}/cross_repo/library-git-submodules-helpers.sh"



function setup_git_repos_a_b_c {

  print_section "Setting up git repo C to be used as submodule in git repo B"
  mkdir "$GIT_REPO_C"
  cd "$GIT_REPO_C" || exit
  git init -q
  echo "choo" > choo
  git add choo
  git commit -q -am "Add choo"
  mkdir hoo
  cd hoo || exit
  echo "qux" > qux
  cd ..
  git add hoo/qux
  git commit -q -am "Add hoo/qux"
  git log --oneline


  print_section "Setting up git repo B to be used as submodule in git repo A"
  mkdir "$GIT_REPO_B"
  cd "$GIT_REPO_B" || exit
  git init -q
  echo "foo" > foo
  git add foo
  git commit -q -am "Add foo"
  mkdir bar
  cd bar || exit
  echo "zoo" > zoo
  cd ..
  git add bar/zoo
  git commit -q -am "Add bar/zoo"
  git submodule add ../git-repo-c

  git add .
  git commit -q -am "Added git repo C as submodule in B"
  git log --oneline

  find . -path "./.git" -prune -o -print | sort



  print_section "Setting up git repo A"
  mkdir "$GIT_REPO_A"
  cd "$GIT_REPO_A" || exit
  git init -q
  echo "root_file" > root_file
  mkdir duplicates
  echo "Same content" > duplicates/x
  echo "Same content" > duplicates/y
  echo "Same content" > duplicates/z
  git add .
  git commit -q -am "Add root_file"
  mkdir regular_dir
  cd regular_dir || exit
  echo "aardvar" > aardvar
  cd ..
  git add regular_dir/aardvar
  git commit -q -am "Add regular_dir/aardvar"
  git submodule add ../git-repo-b

  git add .
  git commit -q -am "Added git repo B as submodule in A"
  git log --oneline

  git submodule add ../git-repo-c repo_c

  git add . && git commit -q -am "Added git repo C as submodule directly in A"

  find . -path "./.git" -prune -o -print | sort


  cd "$TESTTMP" || exit

}


function gitimport_repos_a_b_c {
  # Commit that will be synced after the merge to change the commit sync mapping
  export GIT_REPO_A_HEAD;
  # Commit that will be used in the initial import and merged with large repo's
  # master_bookmark bookmark
  export GIT_REPO_A_HEAD_PARENT;
  print_section "Importing repos in reverse dependency order, C, B then A"

  REPOID="$REPO_C_ID" quiet gitimport "$GIT_REPO_C" --bypass-derived-data-backfilling \
    --bypass-readonly --generate-bookmarks full-repo

  REPOID="$REPO_B_ID" quiet gitimport "$GIT_REPO_B" --bypass-derived-data-backfilling \
    --bypass-readonly --generate-bookmarks full-repo

  # shellcheck disable=SC2153
  REPOID="$SUBMODULE_REPO_ID" gitimport "$GIT_REPO_A" --bypass-derived-data-backfilling \
    --bypass-readonly --generate-bookmarks full-repo > "$TESTTMP/gitimport_output" 2>&1

  GIT_REPO_A_HEAD=$(rg ".*Ref: \"refs/heads/master_bookmark\": Some\(ChangesetId\(Blake2\((\w+).+" -or '$1' "$TESTTMP/gitimport_output")

  GIT_REPO_A_HEAD_PARENT=$(mononoke_admin fetch -R "$SUBMODULE_REPO_NAME" -i "$GIT_REPO_A_HEAD" --json | jq -r .parents[0])


  printf "\nGIT_REPO_A_HEAD: %s\n" "$GIT_REPO_A_HEAD"
  printf "\nGIT_REPO_A_HEAD_PARENT: %s\n" "$GIT_REPO_A_HEAD_PARENT"
}

function merge_repo_a_to_large_repo {
  IMPORT_CONFIG_VERSION_NAME=${NOOP_CONFIG_VERSION_NAME:-$LATEST_CONFIG_VERSION_NAME}
  FINAL_CONFIG_VERSION_NAME=${CONFIG_VERSION_NAME:-$LATEST_CONFIG_VERSION_NAME}
  MASTER_BOOKMARK_NAME=${MASTER_BOOKMARK:-master_bookmark}
  SMALL_REPO_FOLDER=${REPO_A_FOLDER:-$SUBMODULE_REPO_NAME}

  print_section "Importing repo A commits into large repo"

  echo "IMPORT_CONFIG_VERSION_NAME: $IMPORT_CONFIG_VERSION_NAME"
  echo "FINAL_CONFIG_VERSION_NAME: $FINAL_CONFIG_VERSION_NAME"
  echo "Large repo MASTER_BOOKMARK_NAME: $MASTER_BOOKMARK_NAME"
  echo "SMALL_REPO_FOLDER: $SMALL_REPO_FOLDER"

  printf "\nGIT_REPO_A_HEAD: %s\n" "$GIT_REPO_A_HEAD"
  printf "\nGIT_REPO_A_HEAD_PARENT: %s\n" "$GIT_REPO_A_HEAD_PARENT"

  print_section "Running initial import"

  # shellcheck disable=SC2153
  mononoke_x_repo_sync "$SUBMODULE_REPO_ID" "$LARGE_REPO_ID" initial-import \
    --no-progress-bar --derivation-batch-size 2 -i "$GIT_REPO_A_HEAD_PARENT" \
    --version-name "$IMPORT_CONFIG_VERSION_NAME" 2>&1 | tee "$TESTTMP/initial_import_output"

  print_section "Large repo bookmarks"
  mononoke_admin bookmarks -R "$LARGE_REPO_NAME" list -S hg

  IMPORTED_HEAD=$(rg ".+synced as (\w+) in.+" -or '$1' "$TESTTMP/initial_import_output")
  printf "\nIMPORTED_HEAD: %s\n\n" "$IMPORTED_HEAD"

  COMMIT_DATE="1985-09-04T00:00:00.00Z"

  print_section "Creating deletion commits"
  mononoke_admin megarepo gradual-delete \
    --repo-id "$LARGE_REPO_ID" -a test_user -m "deletion commits for merge into large repo" \
    -i "$IMPORTED_HEAD" --even-chunk-size 2 --commit-date-rfc3339 "$COMMIT_DATE" \
    "$SMALL_REPO_FOLDER" 2>&1 | tee "$TESTTMP/gradual_delete.out"

  LAST_DELETION_COMMIT=$(tail -n1 "$TESTTMP/gradual_delete.out")
  printf "\nLAST_DELETION_COMMIT: %s\n\n" "$LAST_DELETION_COMMIT"

  print_section "Creating gradual merge commit"
  mononoke_admin megarepo gradual-merge \
    --repo-id "$LARGE_REPO_ID" -a test_user -m "gradual merge" \
    --last-deletion-commit -i "$LAST_DELETION_COMMIT" \
    --pre-deletion-commit -i "$IMPORTED_HEAD"  --target-bookmark "$MASTER_BOOKMARK_NAME" --limit 10 \
    --commit-date-rfc3339 "$COMMIT_DATE" 2>&1 | tee "$TESTTMP/gradual_merge.out"

  print_section "Changing commit sync mapping version"
  mononoke_x_repo_sync "$SUBMODULE_REPO_ID" "$LARGE_REPO_ID" \
    once --unsafe-force-rewrite-parent-to-target-bookmark --commit-id "$GIT_REPO_A_HEAD" \
    --unsafe-change-version-to "$FINAL_CONFIG_VERSION_NAME" \
    --target-bookmark "$MASTER_BOOKMARK_NAME" 2>&1 | tee "$TESTTMP/xrepo_mapping_change.out"

  SYNCED_HEAD=$(rg ".+synced as (\w+) in.+" -or '$1' "$TESTTMP/xrepo_mapping_change.out")
  printf "\nSYNCED_HEAD: %s\n\n" "$SYNCED_HEAD"

  sleep 2

  clone_and_log_large_repo "$SYNCED_HEAD"

  hg co -q "$MASTER_BOOKMARK_NAME"

  echo "Large repo tree:"
  find . -path "./.hg" -prune -o -print | sort | tee "${TESTTMP}/large_repo_tree_1"


  sleep 2;
  sync_mononoke_warm_bookmarks_cache 2>/dev/null || true
  print_section "Deriving all data types"
  mononoke_admin derived-data -R "$LARGE_REPO_NAME" \
    derive -i "$SYNCED_HEAD" --all-types

  print_section "Count underived data types"
  mononoke_admin derived-data -R "$LARGE_REPO_NAME" \
    count-underived -i "$SYNCED_HEAD" -T fsnodes

  mononoke_admin derived-data -R "$LARGE_REPO_NAME" \
    count-underived -i "$SYNCED_HEAD" -T changeset_info

  mononoke_admin derived-data -R "$LARGE_REPO_NAME" \
    count-underived -i "$SYNCED_HEAD" -T hgchangesets

}

# This will make some changes to all repos, so we can assert that all of them
# are expanded properly and that the submodule pointer update diffs only
# contain the necessary delta (e.g. instead of the entire working copy of
# the new commit).
function make_changes_to_git_repos_a_b_c {
  # These will store the hash of the HEAD commit in each repo after the changes
  export GIT_REPO_A_HEAD;
  export GIT_REPO_B_HEAD;
  export GIT_REPO_C_HEAD;

  print_section "Make changes to repo C"
  cd "$GIT_REPO_C" || exit
  echo 'another file' > choo3 && git add .
  git commit -q -am "commit #3 in repo C"
  echo 'another file' > choo4 && git add .
  git commit -q -am "commit #4 in repo C"
  git log --oneline

  GIT_REPO_C_HEAD=$(git rev-parse HEAD)

  print_section "Update those changes in repo B"
  cd "$GIT_REPO_B" || exit
  git submodule update --remote

  git add .
  git commit -q -am "Update submodule C in repo B"
  rm bar/zoo foo
  git add . && git commit -q -am "Delete files in repo B"
  git log --oneline

  GIT_REPO_B_HEAD=$(git rev-parse HEAD)

  print_section "Update those changes in repo A"
  cd "$GIT_REPO_A" || exit
  # Make simple change directly in repo A
  echo "in A" >> root_file && git add .
  git commit -q -am "Change directly in A"

  print_section "Update submodule b in A"
  git submodule update --remote

  git commit -q -am "Update submodule B in repo A"

  print_section "Then delete repo C submodule used directly in repo A"
  git submodule deinit --force repo_c

  git rm -r repo_c

  git add . && git commit -q -am "Remove repo C submodule from repo A"
  git log --oneline

  GIT_REPO_A_HEAD=$(git rev-parse HEAD)
}




# Create a commit in repo_b that can be used to update its submodule pointer
# from the large repo
function create_repo_b_commits_for_submodule_pointer_update {
  export REPO_B_GIT_COMMIT_HASH;

  print_section "Create a commit in repo_b"
  #  Create a commit in repo_b to update its repo_a pointer from the large repo
  cd "$GIT_REPO_B" || exit
  echo "new file abc" > abc
  git add .
  git commit -q -am "Add file to repo_b"

  cd "$TESTTMP" || exit

  # Import this commit to repo_b mononoke mirror
  REPOID="$REPO_B_ID" gitimport "$GIT_REPO_B" --bypass-derived-data-backfilling  \
    --bypass-readonly --generate-bookmarks full-repo > "$TESTTMP/gitimport_output" 2>&1

  REPO_B_BONSAI=$(rg ".*Ref: \"refs/heads/master_bookmark\": Some\(ChangesetId\(Blake2\((\w+).+" -or '$1' "$TESTTMP/gitimport_output")
  echo "REPO_B_BONSAI: $REPO_B_BONSAI"
  # GIT_REPO_B_HEAD: 3cd7a66e604714b2b96af41e9c595be692f1f5f0713af3f7b2dc3426b05407bd

  REPO_B_GIT_COMMIT_HASH=$(mononoke_admin convert --repo-id "$REPO_B_ID" -f bonsai -t git "$REPO_B_BONSAI")
  echo "REPO_B_GIT_COMMIT_HASH: $REPO_B_GIT_COMMIT_HASH"
  # REPO_B_GIT_HASH: e412b2106ae18eab108f1f8d7ed6e4527d0296cc
}

# Create a commit in repo_b and update its submodule pointer from the large repo
function update_repo_b_submodule_pointer_in_large_repo {
  create_repo_b_commits_for_submodule_pointer_update;

  cd "$TESTTMP/$LARGE_REPO_NAME" || exit
  echo "new file abc" > smallrepofolder1/git-repo-b/abc
  printf "%s" "$REPO_B_GIT_COMMIT_HASH" > smallrepofolder1/.x-repo-submodule-git-repo-b
  hg commit -Aq -m "Valid repo_b submodule version bump from large repo"
  hg cloud backup -q
}

# Create a commit in repo_a.
function create_repo_a_commit {
  export REPO_A_GIT_HASH;

  cd "$GIT_REPO_A" || exit
  date >> file_in_a
  git add .
  git commit -q -am "A commit in repo_a"
  cd "$TESTTMP" || exit

  # Import this commit to repo_a mononoke mirror
  REPOID="$SUBMODULE_REPO_ID" gitimport "$GIT_REPO_A" --bypass-derived-data-backfilling  \
    --bypass-readonly --generate-bookmarks full-repo > "$TESTTMP/gitimport_repo_a_output" 2>&1

  GIT_REPO_A_HEAD=$(rg ".*Ref: \"refs/heads/master_bookmark\": Some\(ChangesetId\(Blake2\((\w+).+" -or '$1' "$TESTTMP/gitimport_repo_a_output")
  echo "GIT_REPO_A_HEAD: $GIT_REPO_A_HEAD"

  REPO_A_GIT_HASH=$(mononoke_admin convert --repo-id "$SUBMODULE_REPO_ID" -f bonsai -t git "$GIT_REPO_A_HEAD")
  echo "REPO_A_GIT_HASH: $REPO_A_GIT_HASH"
}

# Create commits in repo_c and repo_b that can be used to update their submodule
# pointers from the large repo

# Create a commit in repo_c that can be used to update its submodule pointer
# from the large repo
function create_repo_c_commit {
  export REPO_C_GIT_HASH;

  #  Create a commit in repo_b to update its repo_a pointer from the large repo
  cd "$GIT_REPO_C" || exit
  echo "new file in repo_c" > file_in_c
  git add .
  git commit -q -am "Add file in repo_c"
  cd "$TESTTMP" || exit

  # Import this commit to repo_c mononoke mirror
  REPOID="$REPO_C_ID" gitimport "$GIT_REPO_C" --bypass-derived-data-backfilling  \
    --bypass-readonly --generate-bookmarks full-repo > "$TESTTMP/gitimport_repo_c_output" 2>&1

  GIT_REPO_C_HEAD=$(rg ".*Ref: \"refs/heads/master_bookmark\": Some\(ChangesetId\(Blake2\((\w+).+" -or '$1' "$TESTTMP/gitimport_repo_c_output")
  echo "GIT_REPO_C_HEAD: $GIT_REPO_C_HEAD"

  REPO_C_GIT_HASH=$(mononoke_admin convert --repo-id "$REPO_C_ID" -f bonsai -t git "$GIT_REPO_C_HEAD")
  echo "REPO_C_GIT_HASH: $REPO_C_GIT_HASH"
}

# Create commits in repo_c and repo_b that can be used to update their submodule
# pointers from the large repo
function create_repo_c_and_repo_b_commits_for_submodule_pointer_update {
  export REPO_B_GIT_COMMIT_HASH;
  export REPO_C_SUBMODULE_GIT_HASH;


  print_section "Create a commit in repo_c and update its pointer in repo_b"
  create_repo_c_commit;
  REPO_C_SUBMODULE_GIT_HASH="$REPO_C_GIT_HASH";

  print_section "Update repo_c submodule in git repo_b"
  cd "$GIT_REPO_B" || exit
  git submodule update --remote
  git add .
  git commit -q -am "Update submodule C in repo B"

  # Import this commit to repo_b mononoke mirror
  REPOID="$REPO_B_ID" gitimport "$GIT_REPO_B" --bypass-derived-data-backfilling  \
    --bypass-readonly --generate-bookmarks full-repo > "$TESTTMP/gitimport_output" 2>&1

  GIT_REPO_B_HEAD=$(rg ".*Ref: \"refs/heads/master_bookmark\": Some\(ChangesetId\(Blake2\((\w+).+" -or '$1' "$TESTTMP/gitimport_output")
  echo "GIT_REPO_B_HEAD: $GIT_REPO_B_HEAD"

  REPO_B_GIT_COMMIT_HASH=$(mononoke_admin convert --repo-id "$REPO_B_ID" -f bonsai -t git "$GIT_REPO_B_HEAD")
  echo "REPO_B_GIT_COMMIT_HASH: $REPO_B_GIT_COMMIT_HASH"

}

# Create a commit in repo_b and repo_c, then update its submodule pointers from
# the large repo to test recursive submodule updates from the large repo
function update_repo_c_submodule_pointer_in_large_repo {
  create_repo_c_and_repo_b_commits_for_submodule_pointer_update;

  cd "$TESTTMP/$LARGE_REPO_NAME" || exit

  # Update repo_c working copy in repo_b submodule metadata file
  echo "new file in repo_c" > smallrepofolder1/git-repo-b/git-repo-c/file_in_c

  echo "Updating repo_b/repo_c submodule pointer to: $REPO_C_SUBMODULE_GIT_HASH"
  # Update repo_c submodule metadata file
  printf "%s" "$REPO_C_SUBMODULE_GIT_HASH" > smallrepofolder1/git-repo-b/.x-repo-submodule-git-repo-c

  echo "Updating repo_b submodule pointer to: $REPO_B_GIT_COMMIT_HASH"

  # Update repo_b submodule metadata file
  printf "%s" "$REPO_B_GIT_COMMIT_HASH" > smallrepofolder1/.x-repo-submodule-git-repo-b

  hg commit -Aq -m "Valid repo_b and repo_c recursive submodule version bump from large repo"
  hg cloud backup -q
}

function switch_source_of_truth_to_large_repo {
  export LARGE_REPO_BOOKMARK_UPDATE_LOG_ID;
  local small_repo=$1
  local large_repo=$2
  local source_commit target_commit

  orig_pwd=$(pwd)

  # Set the backsyncer mutable counter
  print_section "Get current large repo bookmark update log id to set the backsyncer counter"
  LARGE_REPO_BOOKMARK_UPDATE_LOG_ID=$(mononoke_admin bookmarks \
    --repo-id "$large_repo" log "$MASTER_BOOKMARK_NAME" -S bonsai,hg -l1 \
    | cut -d " " -f1)

  echo "LARGE_REPO_BOOKMARK_UPDATE_LOG_ID: $LARGE_REPO_BOOKMARK_UPDATE_LOG_ID"
  print_section "Set backsyncer counter"
  sqlite3 "$TESTTMP/monsql/sqlite_dbs" \
    "INSERT INTO mutable_counters (repo_id, name, value) \
    VALUES ($small_repo, 'backsync_from_$LARGE_REPO_ID', $LARGE_REPO_BOOKMARK_UPDATE_LOG_ID)";

  BACKSYNC_COUNTER=$(sqlite3 "$TESTTMP/monsql/sqlite_dbs" \
    "SELECT value FROM mutable_counters WHERE name = 'backsync_from_$LARGE_REPO_ID';")
  echo "BACKSYNC_COUNTER: $BACKSYNC_COUNTER"

  source_commit=$(mononoke_admin bookmarks --repo-id "$large_repo" get "$MASTER_BOOKMARK_NAME" -S bonsai) || return
  target_commit=$(mononoke_admin bookmarks --repo-id "$small_repo" get "heads/$MASTER_BOOKMARK_NAME" -S bonsai) || return
  qualify_backsync_cursor_at_fixture_checkpoint "$large_repo" "$small_repo" \
    "$MASTER_BOOKMARK_NAME" "heads/$MASTER_BOOKMARK_NAME" \
    "$source_commit" "$target_commit" || return

  merge_just_knobs <<EOF
{"bools": {"scm/mononoke:backsyncer_prefix_polling": true}}
EOF

  # Start backsyncer in the background
  REPOIDSMALL=$SUBMODULE_REPO_ID REPOIDLARGE=$LARGE_REPO_ID \
    backsync_large_to_small_forever

  # Enable pushredirection for small repo, i.e. switch the source of truth to large repo
  print_section "Enable push redirection for small repo"
  enable_pushredirect "$small_repo" false true

  # Disable the deny_files hook in small repo
  rm "$TESTTMP/mononoke-config/repos/$LARGE_REPO_NAME/server.toml"
  mv "$TESTTMP/old_large_repo_config.toml" "$TESTTMP/mononoke-config/repos/$LARGE_REPO_NAME/server.toml"

  if [[ -z "$SCS_PORT" ]]; then
    start_and_wait_for_scs_server
  fi

  # Enable commit hook that ensures all commits are backsyncable (i.e. doesn't
  # break submodule expansions)
  cat >> "$TESTTMP/mononoke-config/repos/$LARGE_REPO_NAME/server.toml" << CONFIG
[[bookmarks]]
regex=".*"
[[bookmarks.hooks]]
hook_name="check_commit_is_backsyncable"
[[hooks]]
name="check_commit_is_backsyncable"
config_json='''{
  "sync_dir_to_repo_map": {"$SMALL_REPO_DIR": "$SUBMODULE_REPO_NAME"},
  "target_scs": {
    "HostPort": {
      "host": "localhost",
      "port": $SCS_PORT,
      "cert_paths": {
        "cert_path": "$TEST_CERTDIR/${OVERRIDE_CLIENT_CERT:-client0}.crt",
        "key_path": "$TEST_CERTDIR/${OVERRIDE_CLIENT_CERT:-client0}.key",
        "ca_path": "$TEST_CERTDIR/root-ca.crt"
      }
    }
  }
}'''
CONFIG

  # Restart Mononoke and Mononoke Git service to pick up the config change
  killandwait "$MONONOKE_PID"
  start_and_wait_for_mononoke_server

  killandwait "$MONONOKE_GIT_SERVICE_PID"

  mononoke_git_service

  # Update the remote url on the small repo clone if the directory exists
  if [ -d "$SUBMODULE_REPO_GIT" ]; then
    cd "$SUBMODULE_REPO_GIT" || exit
    git remote set-url origin "$MONONOKE_GIT_SERVICE_BASE_URL/$SUBMODULE_REPO_NAME.git"
  fi

  cd "$orig_pwd" || exit
}

# Creates repos B and C, sets up repo A and merges it into the large repo.
function create_and_merge_submodule_repo {
  # Setup git repos A, B and C
  setup_git_repos_a_b_c

  # Import all git repos into Mononoke
  gitimport_repos_a_b_c

  # Merge repo A into the large repo
  merge_repo_a_to_large_repo
}

# - Runs `create_and_merge_submodule_repo`
# - Makes further changes to the repos to ensure they are synced properly
# - Sets up live sync
function setup_all_repos_for_test {
  UPDATE_REPO_A=${UPDATE_REPO_A-1}

  create_and_merge_submodule_repo

  # Make changes to submodule and make sure they're synced properly
  make_changes_to_git_repos_a_b_c

  # Import the changes from the git repos B and C into their Mononoke repos
  REPOID="$REPO_C_ID" QUIET_LOGGING_LOG_FILE="$TESTTMP/gitimport_repo_c.out"  \
   quiet gitimport "$GIT_REPO_C" --bypass-derived-data-backfilling \
   --bypass-readonly --generate-bookmarks missing-for-commit "$GIT_REPO_C_HEAD"

  REPOID="$REPO_B_ID" QUIET_LOGGING_LOG_FILE="$TESTTMP/gitimport_repo_b.out" \
   quiet gitimport "$GIT_REPO_B" --bypass-derived-data-backfilling \
   --bypass-readonly --generate-bookmarks missing-for-commit "$GIT_REPO_B_HEAD"

  # Set up live forward syncer, which should sync all commits in small repo's (repo A)
  # heads/master_bookmark bookmark to large repo's master_bookmark bookmark via pushrebase
  touch "$TESTTMP/xreposync.out"
  mononoke_x_repo_sync_forever "$SUBMODULE_REPO_ID" "$LARGE_REPO_ID"

  # Import the changes from git repo A into its Mononoke repo. They should be automatically
  # forward synced to the large repo
  # TODO: generate or not bookmarks for SCS tests UPDATE_REPO_A
  if [ "$UPDATE_REPO_A" != 0 ]; then
    REPOID="$SUBMODULE_REPO_ID" gitimport "$GIT_REPO_A" --bypass-derived-data-backfilling \
      --bypass-readonly --generate-bookmarks missing-for-commit "$GIT_REPO_A_HEAD" > "$TESTTMP/gitimport_output" 2>&1
  fi

}
