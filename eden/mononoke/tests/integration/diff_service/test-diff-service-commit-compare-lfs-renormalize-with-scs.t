# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for commit_compare over LFS-renormalize commits (SEV
# S657050). Drives commit_compare via SCS routed through diff_service (the
# production path with [remote_diff_config]). Builds the same 4-commit
# stack as the other two .t files in this series, exercising both
# directions: raw_to_lfs.txt (enabling LFS) and lfs_to_raw.txt (disabling
# LFS), with the LFS state on the parent side inherited from the root.

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

Start mononoke (needed for SCS) and the diff service; wire SCS to the
diff service via [remote_diff_config], then start SCS with
--diff-remotely.
  $ mononoke
  $ wait_for_mononoke
  $ start_and_wait_for_diff_service

  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" << CONFIG
  > [remote_diff_config]
  > host_port = "$(diff_service_address)"
  > CONFIG

  $ start_and_wait_for_scs_server --diff-remotely

== Both renormalized paths surface in commit_compare D vs C ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --paths-only --ordered
  A D
  M lfs_to_raw.txt
  M raw_to_lfs.txt

== Body for raw_to_lfs.txt (enabling LFS): unified diff ==
Raw content on the `-` side, LFS pointer text on the `+` side -- matches
`git add --renormalize` for text content.
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "raw_to_lfs.txt"
  diff --git a/raw_to_lfs.txt b/raw_to_lfs.txt
  --- a/raw_to_lfs.txt
  +++ b/raw_to_lfs.txt
  @@ -1,1 +1,3 @@
  -raw content here
  \ No newline at end of file
  +version https://git-lfs.github.com/spec/v1
  +oid sha256:* (glob)
  +size 16

== Body for lfs_to_raw.txt (disabling LFS): unified diff ==
LFS pointer text on the `-` side, raw content on the `+` side -- matches
`git add --renormalize` going the other way.
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "lfs_to_raw.txt"
  diff --git a/lfs_to_raw.txt b/lfs_to_raw.txt
  --- a/lfs_to_raw.txt
  +++ b/lfs_to_raw.txt
  @@ -1,3 +1,1 @@
  -version https://git-lfs.github.com/spec/v1
  -oid sha256:* (glob)
  -size 11
  +shared blob
  \ No newline at end of file

== Verify diff_service was actually invoked for commit_compare ==
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete" and .normal.method == "commit_compare") | .normal.status' | sort | uniq
  SUCCESS

== Unified diffs in an LFS-interpreting repo keep commit paths ==
The diff service reads the LFS pointer from the changeset's file change, so
SCS does not name these files by content ID.
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Computed unified diff" or .normal.log_tag == "Computed unified diff (unary)") | "base:\(if .normal.commit then "commit" elif .normal.base_content_id then "content" else "none" end) other:\(if .normal.other_commit then "commit" elif .normal.other_content_id then "content" else "none" end)"' | sort -u
  base:commit other:commit
