# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ REPONAME="repo" setup_common_config
setup basic repo with some commits and files
  $ testtool_drawdag -R repo <<'EOF'
  > A-B
  > # bookmark: B main
  > # modify: A "file1.txt" "content A\nline 2\nline 3\n"
  > # modify: B "file1.txt" "content B\nline 2\nline 3\n"
  > EOF
  A=* (glob)
  B=* (glob)

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

Get content IDs for mixed input type tests
  $ FILE1_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_B_CONTENT_ID=$(mononoke_admin fetch -R repo -i $B -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

String Input Tests
=====

Test diff hunks with string inputs
  $ diff_service_client diff-hunks --repo-name repo --base-string "line1\nline2\nline3\n" --other-string "line1\nmodified\nline3\n"
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-1)
  === End of hunks ===

Test diff hunks with string inputs (no changes)
  $ diff_service_client diff-hunks --repo-name repo --base-string "same content\n" --other-string "same content\n"
  Hunks output (num_hunks: 0):
  === End of hunks ===

Test diff hunks with mixed input types - string vs content ID
  $ diff_service_client diff-hunks --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-content-id $FILE1_B_CONTENT_ID
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-3), remove_range(0-1)
  === End of hunks ===

Test diff hunks with mixed input types - content ID vs string
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-string "content B\nline 2\nline 3\n"
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-3)
  === End of hunks ===

Test diff hunks with mixed input types - string vs changeset+path
  $ diff_service_client diff-hunks --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-changeset-id $B --other-path file1.txt
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-3), remove_range(0-1)
  === End of hunks ===

Test diff unified with string inputs
  $ diff_service_client diff-unified --repo-name repo --base-string "line1\nline2\nline3\n" --other-string "line1\nmodified\nline3\n"
  Unified diff output (is_binary: false):
  diff --git a/base_path b/other_path
  --- a/base_path
  +++ b/other_path
  @@ -1,1 +1,1 @@
  -line1\nline2\nline3\n
  \ No newline at end of file
  +line1\nmodified\nline3\n
  \ No newline at end of file
  === End of unified diff ===

Test diff unified with string inputs and custom context
  $ diff_service_client diff-unified --repo-name repo --base-string "line1\nline2\nline3\nline4\nline5\n" --other-string "line1\nmodified\nline3\nline4\nline5\n" --context-lines 1
  Unified diff output (is_binary: false):
  diff --git a/base_path b/other_path
  --- a/base_path
  +++ b/other_path
  @@ -1,1 +1,1 @@
  -line1\nline2\nline3\nline4\nline5\n
  \ No newline at end of file
  +line1\nmodified\nline3\nline4\nline5\n
  \ No newline at end of file
  === End of unified diff ===

Test diff unified with string inputs (no changes)
  $ diff_service_client diff-unified --repo-name repo --base-string "same content\n" --other-string "same content\n"
  Unified diff output (is_binary: false):
  === End of unified diff ===

Test diff unified with mixed input types - string vs content ID
  $ diff_service_client diff-unified --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-content-id $FILE1_B_CONTENT_ID --other-content-path "file1.txt"
  Unified diff output (is_binary: false):
  diff --git a/base_path b/file1.txt
  --- a/base_path
  +++ b/file1.txt
  @@ -1,1 +1,3 @@
  -content A\nline 2\nline 3\n
  \ No newline at end of file
  +content B
  +line 2
  +line 3
  === End of unified diff ===

Test diff unified with mixed input types - content ID vs string
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --base-content-path "file1.txt" --other-string "content B\nline 2\nline 3\n"
  Unified diff output (is_binary: false):
  diff --git a/file1.txt b/other_path
  --- a/file1.txt
  +++ b/other_path
  @@ -1,3 +1,1 @@
  -content A
  -line 2
  -line 3
  +content B\nline 2\nline 3\n
  \ No newline at end of file
  === End of unified diff ===

Test diff unified with mixed input types - string vs changeset+path
  $ diff_service_client diff-unified --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-changeset-id $B --other-path file1.txt
  Unified diff output (is_binary: false):
  diff --git a/base_path b/file1.txt
  --- a/base_path
  +++ b/file1.txt
  @@ -1,1 +1,3 @@
  -content A\nline 2\nline 3\n
  \ No newline at end of file
  +content B
  +line 2
  +line 3
  === End of unified diff ===

Test diff unified headerless with string inputs
  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "line1\nline2\nline3\n" --other-string "line1\nmodified\nline3\n"
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,1 +1,1 @@
  -line1\nmodified\nline3\n
  \ No newline at end of file
  +line1\nline2\nline3\n
  \ No newline at end of file
  === End of headerless diff ===

Test diff unified headerless with string inputs and custom context
  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "line1\nline2\nline3\nline4\nline5\n" --other-string "line1\nmodified\nline3\nline4\nline5\n" --context-lines 1
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,1 +1,1 @@
  -line1\nmodified\nline3\nline4\nline5\n
  \ No newline at end of file
  +line1\nline2\nline3\nline4\nline5\n
  \ No newline at end of file
  === End of headerless diff ===

Test diff unified headerless with string inputs (no changes)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "same content\n" --other-string "same content\n"
  Headerless diff output (is_binary: false, num_chunks: 0):
  === End of headerless diff ===

Test diff unified headerless with mixed input types - string vs content ID
  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-content-id $FILE1_B_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,1 @@
  -content B
  -line 2
  -line 3
  +content A\nline 2\nline 3\n
  \ No newline at end of file
  === End of headerless diff ===

Test diff unified headerless with mixed input types - content ID vs string
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-string "content B\nline 2\nline 3\n"
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,1 +1,3 @@
  -content B\nline 2\nline 3\n
  \ No newline at end of file
  +content A
  +line 2
  +line 3
  === End of headerless diff ===

Test diff unified headerless with mixed input types - string vs changeset+path
  $ diff_service_client diff-unified-headerless --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-changeset-id $B --other-path file1.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,1 @@
  -content B
  -line 2
  -line 3
  +content A\nline 2\nline 3\n
  \ No newline at end of file
  === End of headerless diff ===

Test metadata diff with string inputs
  $ diff_service_client metadata-diff --repo-name repo --base-string "line1\nline2\nline3\n" --other-string "line1\nmodified\nline3\n"
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 1
    Deleted lines: 1
    Significant added lines: 1
    Significant deleted lines: 1
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with string inputs (identical)
  $ diff_service_client metadata-diff --repo-name repo --base-string "same content\n" --other-string "same content\n"
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 0
    Deleted lines: 0
    Significant added lines: 0
    Significant deleted lines: 0
  === End of metadata diff ===

Test metadata diff with mixed input types - string vs content ID
  $ diff_service_client metadata-diff --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-content-id $FILE1_B_CONTENT_ID
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 3
    Deleted lines: 1
    Significant added lines: 3
    Significant deleted lines: 1
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with mixed input types - content ID vs string
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-string "content B\nline 2\nline 3\n"
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 1
    Deleted lines: 3
    Significant added lines: 1
    Significant deleted lines: 3
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with mixed input types - string vs changeset+path
  $ diff_service_client metadata-diff --repo-name repo --base-string "content A\nline 2\nline 3\n" --other-changeset-id $B --other-path file1.txt
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 3
    Deleted lines: 1
    Significant added lines: 3
    Significant deleted lines: 1
    First added line number: 1
  === End of metadata diff ===

Test error handling with missing string inputs
  $ diff_service_client diff-hunks --repo-name repo --base-string "content"
  Error: Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string
  [1]
