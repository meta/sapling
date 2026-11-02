# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ REPONAME="repo" setup_common_config

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

Ignore Whitespace Tests
=====

Setup files with whitespace differences
  $ testtool_drawdag -R repo <<'EOF'
  > W-X
  > # bookmark: X whitespace_main
  > # modify: W "ws_file.txt" "line1\nline2\nline3\n"
  > # modify: X "ws_file.txt" "line1\n  line2\nline3\n"
  > EOF
  W=* (glob)
  X=* (glob)

  $ WS_FILE_W_CONTENT_ID=$(mononoke_admin fetch -R repo -i $W -p ws_file.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ WS_FILE_X_CONTENT_ID=$(mononoke_admin fetch -R repo -i $X -p ws_file.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test diff hunks with ignore_whitespace
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID
  Hunks output (num_hunks: 1):
  Hunk: add_range(1-2), remove_range(1-2)
  === End of hunks ===

  $ diff_service_client diff-hunks --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID --ignore-whitespace
  Hunks output (num_hunks: 0):
  === End of hunks ===

Test diff unified with ignore_whitespace
  $ diff_service_client diff-unified --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID --base-content-path "ws_file.txt" --other-content-path "ws_file.txt"
  Unified diff output (is_binary: false):
  * (glob)
  --- a/ws_file.txt
  +++ b/ws_file.txt
  @@ -1,3 +1,3 @@
   line1
  -line2
  +  line2
   line3
  === End of unified diff ===

  $ diff_service_client diff-unified --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID --base-content-path "ws_file.txt" --other-content-path "ws_file.txt" --ignore-whitespace
  Unified diff output (is_binary: false):
  === End of unified diff ===

Test diff unified headerless with ignore_whitespace
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
   line1
  -  line2
  +line2
   line3
  === End of headerless diff ===

  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID --ignore-whitespace
  Headerless diff output (is_binary: false, num_chunks: 0):
  === End of headerless diff ===

Test metadata diff with ignore_whitespace
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 1
    Deleted lines: 1
    Significant added lines: 1
    Significant deleted lines: 1
    First added line number: 2
  === End of metadata diff ===

  $ diff_service_client metadata-diff --repo-name repo --base-content-id $WS_FILE_W_CONTENT_ID --other-content-id $WS_FILE_X_CONTENT_ID --ignore-whitespace
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 0
    Deleted lines: 0
    Significant added lines: 0
    Significant deleted lines: 0
  === End of metadata diff ===

With ignore_whitespace, changed lines keep their whitespace and unchanged lines
are printed from the old input
  $ testtool_drawdag -R repo <<'EOF'
  > Y-Z
  > # modify: Y "ws_mixed.txt" "  x\na a\nb b\n"
  > # modify: Z "ws_mixed.txt" "x\nab\nbb\n"
  > EOF
  Y=* (glob)
  Z=* (glob)

  $ WS_MIXED_Y_CONTENT_ID=$(mononoke_admin fetch -R repo -i $Y -p ws_mixed.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ WS_MIXED_Z_CONTENT_ID=$(mononoke_admin fetch -R repo -i $Z -p ws_mixed.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

  $ diff_service_client diff-unified --repo-name repo --base-content-id $WS_MIXED_Y_CONTENT_ID --other-content-id $WS_MIXED_Z_CONTENT_ID --base-content-path "ws_mixed.txt" --other-content-path "ws_mixed.txt" --ignore-whitespace
  Unified diff output (is_binary: false):
  diff --git a/ws_mixed.txt b/ws_mixed.txt
  --- a/ws_mixed.txt
  +++ b/ws_mixed.txt
  @@ -1,3 +1,3 @@
     x
  -a a
  +ab
   b b
  === End of unified diff ===

The headerless diff takes the other input as the old one; with the inputs
swapped it prints the same hunk
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $WS_MIXED_Z_CONTENT_ID --other-content-id $WS_MIXED_Y_CONTENT_ID --ignore-whitespace
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
     x
  -a a
  +ab
   b b
  === End of headerless diff ===

The examples from T265808302, sent as strings to the headerless diff as www does
  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "ab" --other-string "a a" --ignore-whitespace
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,1 +1,1 @@
  -a a
  \ No newline at end of file
  +ab
  \ No newline at end of file
  === End of headerless diff ===

  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "aa" --other-string "a a" --ignore-whitespace
  Headerless diff output (is_binary: false, num_chunks: 0):
  === End of headerless diff ===
