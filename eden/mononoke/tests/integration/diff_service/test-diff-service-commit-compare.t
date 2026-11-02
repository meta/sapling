# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for diff_service commit_compare handler
# Tests the commit_compare operation directly via diff_service_client

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ REPONAME="repo" setup_common_config

Create a test repository with files to compare
  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C
  > # bookmark: C main
  > # modify: A "file1.txt" "line 1\nline 2\nline 3\n"
  > # modify: B "file1.txt" "line 1\nmodified\nline 3\n"
  > # modify: B "new_file.txt" "new content\n"
  > # copy: C "copied.txt" "copied content\n" B "file1.txt"
  > # delete: C "new_file.txt"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

== Test basic file diff ==

Compare B vs A - should show modified file1.txt and added new_file.txt
  $ diff_service_client commit-compare -R repo --commit-id $B --other-commit-id $A --paths-only | sort
  A B
  A new_file.txt
  M file1.txt

== Test copy detection ==

Compare C vs B - should show copied.txt and deleted new_file.txt
  $ diff_service_client commit-compare -R repo --commit-id $C --other-commit-id $B --paths-only | sort
  A C
  C file1.txt -> copied.txt
  D new_file.txt

== Test auto-parent (no other_commit_id) ==

When no other_commit_id is provided, should diff against parent
  $ diff_service_client commit-compare -R repo --commit-id $B --paths-only | sort
  A B
  A new_file.txt
  M file1.txt

== Test tree changes ==

Compare with --compare-trees flag
  $ diff_service_client commit-compare -R repo --commit-id $B --other-commit-id $A --compare-trees --paths-only | sort
  A B
  A new_file.txt
  M  (tree)
  M file1.txt

== Test verbose output (without --paths-only) ==

  $ diff_service_client commit-compare -R repo --commit-id $B --other-commit-id $A 2>&1 | head -5
  Changed files (3):
  * (glob)
  * (glob)
  * (glob)
  Other commit IDs:

== Verify diff service logged commit_compare ==

  $ cat "$TESTTMP/diff-service.json" | jq -r '.normal.method' | grep -v null | sort | uniq
  commit_compare

All requests should have succeeded
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete") | .normal.status' | sort | uniq
  SUCCESS
