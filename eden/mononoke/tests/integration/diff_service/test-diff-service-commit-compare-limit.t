# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for commit_compare unordered diff limit enforcement.
# Verifies that oversized unordered diffs are rejected with a RequestError
# (not InternalError) and that the error message propagates correctly.

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ REPONAME="repo" setup_common_config

Create a test repository with many files to exceed the limit
  $ testtool_drawdag -R repo <<'EOF'
  > A-B
  > # bookmark: B main
  > # modify: A "base.txt" "base content\n"
  > # modify: B "file1.txt" "c1\n"
  > # modify: B "file2.txt" "c2\n"
  > # modify: B "file3.txt" "c3\n"
  > # modify: B "file4.txt" "c4\n"
  > # modify: B "file5.txt" "c5\n"
  > EOF
  A=* (glob)
  B=* (glob)

Override the unordered max paths limit to 3 (below the 5 new files in B)
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:commit_compare_unordered_max_paths": 3
  >   }
  > }
  > EOF

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

== Test: oversized unordered diff is rejected with RequestError ==

Compare B vs A with 5 file changes but a limit of 3
  $ diff_service_client commit-compare -R repo --commit-id $B --other-commit-id $A --paths-only 2>&1
  Error: DiffService::commit_compare failed with ex(RequestError)
  
  Caused by:
      RequestError { reason: diff_error(DiffError { reason: "commit_compare: unordered diff has more than 3 entries. Use ordered_params with pagination instead." }) }
  [1]
