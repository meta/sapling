# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for commit_compare over LFS-renormalize commits (SEV
# S657050). Drives commit_compare directly via diff_service_client. Builds
# a 4-commit stack A-B-C-D where D's parent C has no FileChange entry for
# either renormalized path -- LFS state is inherited from A through B and
# C -- exercising the orphan-cleanup scenario in both directions:
#   - raw_to_lfs.txt: raw at A, inherited, then re-encoded as an LFS
#     pointer at D (enabling LFS).
#   - lfs_to_raw.txt: LFS pointer at A, inherited, then re-encoded as raw
#     at D (disabling LFS).

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ GIT_LFS_INTERPRET_POINTERS=1 REPONAME="repo" setup_common_config

Build the renormalize stack. Resolved content is identical on both sides
for both renormalized paths.
  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C-D
  > # bookmark: D main
  > # modify: A "raw_to_lfs.txt" "raw content here"
  > # modify: A "lfs_to_raw.txt" regular lfs_pointer "shared blob"
  > # modify: A "anchor.txt" "anchor A"
  > # modify: B "anchor.txt" "anchor B"
  > # modify: C "anchor.txt" "anchor C"
  > # modify: D "raw_to_lfs.txt" regular lfs_pointer "raw content here"
  > # modify: D "lfs_to_raw.txt" "shared blob"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

== Both renormalized paths surface in commit_compare D vs C ==
  $ diff_service_client commit-compare -R repo --commit-id $D --other-commit-id $C --paths-only | sort
  A D
  M lfs_to_raw.txt
  M raw_to_lfs.txt

== Body for raw_to_lfs.txt (enabling LFS): unified diff ==
Raw content on the `-` side, LFS pointer text on the `+` side -- matches
`git add --renormalize` for text content.
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id "$C" --base-path "raw_to_lfs.txt" --other-changeset-id "$D" --other-path "raw_to_lfs.txt"
  Unified diff output (is_binary: false):
  diff --git a/raw_to_lfs.txt b/raw_to_lfs.txt
  --- a/raw_to_lfs.txt
  +++ b/raw_to_lfs.txt
  @@ -1,1 +1,3 @@
  -raw content here
  \ No newline at end of file
  +version https://git-lfs.github.com/spec/v1
  +oid sha256:* (glob)
  +size 16
  === End of unified diff ===

== Body for lfs_to_raw.txt (disabling LFS): unified diff ==
LFS pointer text on the `-` side, raw content on the `+` side -- matches
`git add --renormalize` going the other way.
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id "$C" --base-path "lfs_to_raw.txt" --other-changeset-id "$D" --other-path "lfs_to_raw.txt"
  Unified diff output (is_binary: false):
  diff --git a/lfs_to_raw.txt b/lfs_to_raw.txt
  --- a/lfs_to_raw.txt
  +++ b/lfs_to_raw.txt
  @@ -1,3 +1,1 @@
  -version https://git-lfs.github.com/spec/v1
  -oid sha256:* (glob)
  -size 11
  +shared blob
  \ No newline at end of file
  === End of unified diff ===

== Verify diff_service logged a successful commit_compare invocation ==
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete" and .normal.method == "commit_compare") | .normal.status' | sort | uniq
  SUCCESS
