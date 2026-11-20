# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ export COMMIT_SCRIBE_CATEGORY=mononoke_commits
  $ export BOOKMARK_SCRIBE_CATEGORY=mononoke_bookmark
  $ export WBC_SCRIBE_CATEGORY=mononoke_bookmark
  $ export MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY=$TESTTMP/scribe_logs/
  $ . "${TEST_FIXTURES}/library.sh"
  $ REPOTYPE="blob_files"
  $ setup_common_config $REPOTYPE
  $ GIT_REPO_ORIGIN="${TESTTMP}/origin/repo-git"
  $ GIT_REPO="${TESTTMP}/repo-git"

  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:wbc_update_by_scribe_tailer": true
  >   }
  > }
  > EOF

# Setup git repository
  $ mkdir -p "$GIT_REPO_ORIGIN"
  $ cd "$GIT_REPO_ORIGIN"
  $ git init -q
  $ echo "this is file1" > file1
  $ git add file1
  $ git commit -qam "Add file1"
  $ old_head=$(git rev-parse HEAD)
  $ git tag -a -m "new tag" first_tag
  $ echo "this is file2" > file2
  $ git add file2
  $ git commit -qam "Add file2"
  $ git tag -a empty_tag -m ""
  $ cd "$TESTTMP"
  $ git clone "$GIT_REPO_ORIGIN"
  Cloning into 'repo-git'...
  done.

# Enable logging of bookmark updates
  $ mkdir -p $TESTTMP/scribe_logs
  $ touch $TESTTMP/scribe_logs/$BOOKMARK_SCRIBE_CATEGORY

# Import it into Mononoke
  $ cd "$TESTTMP"
  $ quiet gitimport "$GIT_REPO" --derive-hg --generate-bookmarks full-repo

# Set Mononoke as the Source of Truth
  $ set_mononoke_as_source_of_truth_for_git

# Start up the Mononoke Git Service
  $ mononoke_git_service
# Clone the Git repo from Mononoke
  $ quiet git_client clone $MONONOKE_GIT_SERVICE_BASE_URL/$REPONAME.git

# Add some new commits to the cloned repo and push it to remote
  $ cd repo
  $ echo "newly added file" > new_file
  $ git add .
  $ git commit -qam "Commit with newly added file"
  $ echo "file 2" > file_2
  $ git add .
  $ git commit -qam "Commit 2"
  $ echo "file 3" > file_3
  $ git add .
  $ git commit -qam "Commit 3"
  $ echo "file 4" > file_4
  $ git add .
  $ git commit -qam "Commit 4"
  $ echo "file 5" > file_5
  $ git add .
  $ git commit -qam "Commit 5"
  $ echo "file 6" > file_6
  $ git add .
  $ git commit -qam "Commit 6"
# Incrementally publish master_bookmark changes through multiple pushes
  $ git branch -f pusher_branch master_bookmark~6
  $ git_client push -f origin pusher_branch
  To https://localhost:$LOCAL_PORT/repos/git/ro/repo.git
   * [new branch]      pusher_branch -> pusher_branch
  $ git branch -f pusher_branch master_bookmark~3
  $ git_client push -f origin pusher_branch
  To https://localhost:$LOCAL_PORT/repos/git/ro/repo.git
     e8615d6..70faae0  pusher_branch -> pusher_branch
  $ current_head=$(git rev-parse pusher_branch)
  $ git branch -f pusher_branch master_bookmark
  $ git_client push -f origin pusher_branch
  To https://localhost:$LOCAL_PORT/repos/git/ro/repo.git
     70faae0..5d04bf5  pusher_branch -> pusher_branch

# Wait for the warm bookmark cache to catch up with the latest changes
  $ wait_for_git_bookmark_move refs/heads/pusher_branch "$current_head" "$(git rev-parse pusher_branch)"

# Cloning the repo in a new folder will not get the latest changes since we didn't really accept the push
  $ cd "$TESTTMP"
  $ quiet git_client clone $MONONOKE_GIT_SERVICE_BASE_URL/$REPONAME.git new_repo
  $ cd new_repo

# List all the known refs. Ensure that the new branches and tags are present in the repo
  $ git show-ref | sort
  5d04bf5a8538644ca808a1436dc00c435f75a65a refs/remotes/origin/pusher_branch
  8963e1f55d1346a07c3aec8c8fc72bf87d0452b1 refs/tags/first_tag
  e8615d6f149b876be0a2f30a1c5bf0c42bf8e136 refs/heads/master_bookmark
  e8615d6f149b876be0a2f30a1c5bf0c42bf8e136 refs/remotes/origin/HEAD
  e8615d6f149b876be0a2f30a1c5bf0c42bf8e136 refs/remotes/origin/master_bookmark
  fb02ed046a1e75fe2abb8763f7c715496ae36353 refs/tags/empty_tag
