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
  > A-B-C-H-I
  > # bookmark: I main
  > # modify: A "file1.txt" "content A\nline 2\nline 3\n"
  > # modify: B "file1.txt" "content B\nline 2\nline 3\n"
  > # modify: C "file1.txt" "content C\nline 2\nline 3\nline 4\nline 5\nline 6\n"
  > # modify: A "file3.txt" "content A\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n"
  > # modify: B "file3.txt" "content B\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n"
  > # modify: C "file3.txt" "content C\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nchanged line 10\nnew line 11\nnew line 12\n"
  > # modify: A "file2.txt" "original content\n"
  > # modify: B "file2.txt" "original content\n"
  > # modify: H "file1.txt" "content H\nline 2\nline 3\nline 4\nline 5\nline 6\n"
  > # modify: H "created_file.txt" "This is a newly created file\nwith multiple lines\nfor testing\n"
  > # delete: I "file2.txt"
  > # modify: I "file1.txt" "content I\nline 2\nline 3\nline 4\nline 5\nline 6\n"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  H=* (glob)
  I=* (glob)

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

test diff hunks method with actual content IDs
Get content IDs for the files we created
  $ FILE1_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_B_CONTENT_ID=$(mononoke_admin fetch -R repo -i $B -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_C_CONTENT_ID=$(mononoke_admin fetch -R repo -i $C -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE2_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file2.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE2_B_CONTENT_ID=$(mononoke_admin fetch -R repo -i $B -p file2.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE3_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file3.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE3_C_CONTENT_ID=$(mononoke_admin fetch -R repo -i $C -p file3.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ CREATED_FILE_H_CONTENT_ID=$(mononoke_admin fetch -R repo -i $H -p created_file.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test diff hunks between different versions of file1.txt
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-1)
  === End of hunks ===

Test diff hunks between A and C (larger diff)
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_C_CONTENT_ID
  Hunks output (num_hunks: 2):
  Hunk: add_range(0-1), remove_range(0-1)
  Hunk: add_range(3-6), remove_range(3-3)
  === End of hunks ===

Test with identical content (should have no hunks)
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_A_CONTENT_ID
  Hunks output (num_hunks: 0):
  === End of hunks ===

Test with identical content from different contentIds (should have no hunks)
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $FILE2_A_CONTENT_ID --other-content-id $FILE2_B_CONTENT_ID
  Hunks output (num_hunks: 0):
  === End of hunks ===

Test error handling with non-existent content ID
  $ diff_service_client diff-hunks --repo-name repo --base-content-id 1111111111111111111111111111111111111111111111111111111111111111 --other-content-id $FILE1_A_CONTENT_ID
  Error: DiffService::diff_hunks failed with ex(RequestError)
  * (glob)
  Caused by:
      RequestError { reason: diff_error(DiffError { reason: "Content not found: *" }) } (glob)
  [1]

Test mixed input types - content ID vs changeset+path
  $ diff_service_client diff-hunks --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-changeset-id $B --other-path file1.txt
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-1)
  === End of hunks ===

Test mixed input types - changeset+path vs content ID
  $ diff_service_client diff-hunks --repo-name repo --base-changeset-id $A --base-path file1.txt --other-content-id $FILE1_C_CONTENT_ID
  Hunks output (num_hunks: 2):
  Hunk: add_range(0-1), remove_range(0-1)
  Hunk: add_range(3-6), remove_range(3-3)
  === End of hunks ===

Test changeset+path inputs (homogeneous)
  $ diff_service_client diff-hunks --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $C --other-path file1.txt
  Hunks output (num_hunks: 2):
  Hunk: add_range(0-1), remove_range(0-1)
  Hunk: add_range(3-6), remove_range(3-3)
  === End of hunks ===
test diff unified method with actual content IDs

Test unified diff with custom context lines (1 line of context)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 1 --base-content-path "file3.txt" --other-content-path "file3.txt"
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file3.txt
  +++ b/file3.txt
  @@ -1,2 +1,2 @@
  -content A
  +content C
   line 2
  @@ -9,2 +9,4 @@
   line 9
  -line 10
  +changed line 10
  +new line 11
  +new line 12
  === End of unified diff ===

Test unified diff with custom context lines (2 lines of context)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 2 --base-content-path "file3.txt" --other-content-path "file3.txt"
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file3.txt
  +++ b/file3.txt
  @@ -1,3 +1,3 @@
  -content A
  +content C
   line 2
   line 3
  @@ -8,3 +8,5 @@
   line 8
   line 9
  -line 10
  +changed line 10
  +new line 11
  +new line 12
  === End of unified diff ===

Test unified diff with custom context lines (3 lines of context)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 3 --base-content-path "file3.txt" --other-content-path "file3.txt"
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file3.txt
  +++ b/file3.txt
  @@ -1,4 +1,4 @@
  -content A
  +content C
   line 2
   line 3
   line 4
  @@ -7,4 +7,6 @@
   line 7
   line 8
   line 9
  -line 10
  +changed line 10
  +new line 11
  +new line 12
  === End of unified diff ===

Test unified diff with changeset input
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt
  * (glob)
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with changeset input and replacement_base_path
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt --replacement-base-path "custom/changeset/path.txt"
  * (glob)
  diff --git a/custom/changeset/path.txt b/file1.txt
  --- a/custom/changeset/path.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff error handling with non-existent content ID
  $ diff_service_client diff-unified --repo-name repo --base-content-id 1111111111111111111111111111111111111111111111111111111111111111 --base-content-path file1.txt --other-content-id $FILE1_A_CONTENT_ID --other-content-path file1.txt
  Error: DiffService::diff_unified failed with ex(RequestError)
  * (glob)
  Caused by:
      RequestError { reason: diff_error(DiffError { reason: "Content not found: *" }) } (glob)
  [1]

Test unified diff error handling with invalid arguments
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID
  Error: Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string
  [1]

Test unified diff with inspect_lfs_pointers flag
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --inspect-lfs-pointers
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with inspect_binary_diffs flag
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt"
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with LFS pointers
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --base-lfs-sha256 "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef" --base-lfs-size 1024 --other-lfs-sha256 "fedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321" --other-lfs-size 2048
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
   version https://git-lfs.github.com/spec/v1
  -oid sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef
  -size 1024
  +oid sha256:fedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321
  +size 2048
  === End of unified diff ===

Test unified diff with LFS pointers, with LFS inspection
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --base-lfs-sha256 "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef" --base-lfs-size 1024 --other-lfs-sha256 "fedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321" --other-lfs-size 2048 --inspect-lfs-pointers
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with diff_mode omit_content
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --diff-mode omit_content
  Unified diff output (is_binary: false):
  * (glob)
  Binary file file1.txt has changed
  === End of unified diff ===

Test unified diff with file_type executable
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --file-type executable
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with copy_info move
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "old_file.txt" --other-content-path "new_file.txt" --copy-info move
  Unified diff output (is_binary: false):
  * (glob)
  rename from old_file.txt
  rename to new_file.txt
  --- a/old_file.txt
  +++ b/new_file.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with multiple options combined
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_C_CONTENT_ID --base-content-path "source.txt" --other-content-path "dest.txt" --context-lines 1 --file-type executable --copy-info copy --inspect-lfs-pointers
  Unified diff output (is_binary: false):
  * (glob)
  copy from source.txt
  copy to dest.txt
  --- a/source.txt
  +++ b/dest.txt
  @@ -1,3 +1,6 @@
  -content A
  +content C
   line 2
   line 3
  +line 4
  +line 5
  +line 6
  === End of unified diff ===

Test unified diff with changeset input and options
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt --context-lines 2 --file-type regular --copy-info none --inspect-lfs-pointers
  * (glob)
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with mixed input types - content ID vs changeset+path
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --base-content-path "file1.txt" --other-changeset-id $B --other-path file1.txt
  Unified diff output (is_binary: false):
  * (glob)
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unified diff ===

Test unified diff with mixed input types - changeset+path vs content ID
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $A --base-path file1.txt --other-content-id $FILE1_C_CONTENT_ID --other-content-path "file1.txt"
  * (glob)
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,6 @@
  -content A
  +content C
   line 2
   line 3
  +line 4
  +line 5
  +line 6
  === End of unified diff ===

Test error handling with invalid diff_mode
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --diff-mode invalid
  Error: Invalid diff_mode: invalid
  [1]

Test error handling with invalid file_type
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --file-type invalid
  Error: Invalid file_type: invalid
  [1]

Test error handling with invalid copy_info
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --copy-info invalid
  Error: Invalid copy_info: invalid
  [1]

Test LFS pointer with only sha256 (missing size)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --base-lfs-sha256 "abc123def456"
  Error: --base-lfs-size is required when --base-lfs-sha256 is provided
  [1]

Test LFS pointer with only size (missing sha256)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --base-lfs-size 1024
  Error: --base-lfs-sha256 is required when --base-lfs-size is provided
  [1]

Test unified diff for file creation - file exists only in other changeset (created_file.txt was created in H)
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $C --base-path created_file.txt --other-changeset-id $H --other-path created_file.txt
  * (glob)
  diff --git a/created_file.txt b/created_file.txt
  new file mode 100644
  --- /dev/null
  +++ b/created_file.txt
  @@ -0,0 +1,3 @@
  +This is a newly created file
  +with multiple lines
  +for testing
  === End of unified diff ===

Test unified diff for file deletion - file exists only in base changeset (file2.txt was deleted in I)
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $H --base-path file2.txt --other-changeset-id $I --other-path file2.txt
  * (glob)
  diff --git a/file2.txt b/file2.txt
  deleted file mode 100644
  --- a/file2.txt
  +++ /dev/null
  @@ -1,1 +0,0 @@
  -original content
  === End of unified diff ===

Git Submodule Tests
=====

Setup repository with Git submodules
  $ testtool_drawdag -R repo <<'EOF'
  > D-E-F-G
  > # bookmark: G submodule_main
  > # modify: D "regular_file.txt" "regular content\n"
  > # modify: D "submodules/repo_a" git-submodule &aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  > # modify: E "submodules/repo_a" git-submodule &bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  > # modify: E "submodules/repo_b" git-submodule &cccccccccccccccccccccccccccccccccccccccc
  > # modify: F "regular_file.txt" "updated regular content\n"
  > # modify: F "submodules/repo_a" git-submodule &dddddddddddddddddddddddddddddddddddddddd
  > # delete: G "submodules/repo_b"
  > # modify: G "submodules/repo_c" git-submodule &eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
  > EOF
  D=* (glob)
  E=* (glob)
  F=* (glob)
  G=* (glob)

Get content IDs for Git submodule files
  $ SUBMODULE_A_D_CONTENT_ID=$(mononoke_admin fetch -R repo -i $D -p submodules/repo_a -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ SUBMODULE_A_E_CONTENT_ID=$(mononoke_admin fetch -R repo -i $E -p submodules/repo_a -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ SUBMODULE_A_F_CONTENT_ID=$(mononoke_admin fetch -R repo -i $F -p submodules/repo_a -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ SUBMODULE_B_E_CONTENT_ID=$(mononoke_admin fetch -R repo -i $E -p submodules/repo_b -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ SUBMODULE_C_G_CONTENT_ID=$(mononoke_admin fetch -R repo -i $G -p submodules/repo_c -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test Git submodule diff with content IDs - submodule update
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_E_CONTENT_ID --base-content-path "submodules/repo_a" --other-content-path "submodules/repo_a" --file-type git_submodule
  Unified diff output (is_binary: false):
  * (glob)
  --- a/submodules/repo_a
  +++ b/submodules/repo_a
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with content IDs - another submodule update
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_E_CONTENT_ID --other-content-id $SUBMODULE_A_F_CONTENT_ID --base-content-path "submodules/repo_a" --other-content-path "submodules/repo_a" --file-type git_submodule
  Unified diff output (is_binary: false):
  * (glob)
  --- a/submodules/repo_a
  +++ b/submodules/repo_a
  @@ -1 +1 @@
  -Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  +Subproject commit dddddddddddddddddddddddddddddddddddddddd
  === End of unified diff ===

Test Git submodule diff with changeset input - submodule update
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $D --base-path submodules/repo_a --other-changeset-id $E --other-path submodules/repo_a --file-type git_submodule
  * (glob)
  diff --git a/submodules/repo_a b/submodules/repo_a
  --- a/submodules/repo_a
  +++ b/submodules/repo_a
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with changeset input - submodule addition
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $D --base-path submodules/repo_b --other-changeset-id $E --other-path submodules/repo_b --file-type git_submodule
  * (glob)
  diff --git a/submodules/repo_b b/submodules/repo_b
  new file mode 160000
  --- /dev/null
  +++ b/submodules/repo_b
  @@ -0,0 +1 @@
  +Subproject commit cccccccccccccccccccccccccccccccccccccccc
  === End of unified diff ===

Test Git submodule diff with changeset input - submodule deletion
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $F --base-path submodules/repo_b --other-changeset-id $G --other-path submodules/repo_b --file-type git_submodule
  * (glob)
  diff --git a/submodules/repo_b b/submodules/repo_b
  deleted file mode 160000
  --- a/submodules/repo_b
  +++ /dev/null
  @@ -1 +0,0 @@
  -Subproject commit cccccccccccccccccccccccccccccccccccccccc
  === End of unified diff ===

Test Git submodule diff with omit_content mode
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_E_CONTENT_ID --base-content-path "submodules/repo_a" --other-content-path "submodules/repo_a" --file-type git_submodule --diff-mode omit_content
  Unified diff output (is_binary: false):
  * (glob)
  --- a/submodules/repo_a
  +++ b/submodules/repo_a
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with copy_info move
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_E_CONTENT_ID --base-content-path "old_submodule_path" --other-content-path "new_submodule_path" --file-type git_submodule --copy-info move
  Unified diff output (is_binary: false):
  * (glob)
  rename from old_submodule_path
  rename to new_submodule_path
  --- a/old_submodule_path
  +++ b/new_submodule_path
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with copy_info copy
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_E_CONTENT_ID --base-content-path "source_submodule" --other-content-path "dest_submodule" --file-type git_submodule --copy-info copy
  Unified diff output (is_binary: false):
  * (glob)
  copy from source_submodule
  copy to dest_submodule
  --- a/source_submodule
  +++ b/dest_submodule
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with custom context lines
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_E_CONTENT_ID --base-content-path "submodules/repo_a" --other-content-path "submodules/repo_a" --file-type git_submodule --context-lines 0
  Unified diff output (is_binary: false):
  * (glob)
  --- a/submodules/repo_a
  +++ b/submodules/repo_a
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with replacement_base_path
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id $D --base-path submodules/repo_a --other-changeset-id $E --other-path submodules/repo_a --file-type git_submodule --replacement-base-path "custom/submodule/path"
  * (glob)
  diff --git a/custom/submodule/path b/submodules/repo_a
  --- a/custom/submodule/path
  +++ b/submodules/repo_a
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  === End of unified diff ===

Test Git submodule diff with identical content (should have no changes)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_D_CONTENT_ID --base-content-path "submodules/repo_a" --other-content-path "submodules/repo_a" --file-type git_submodule
  Unified diff output (is_binary: false):
  * (glob)

Test Git submodule with multiple options combined
  $ diff_service_client diff-unified --repo-name repo --base-content-id $SUBMODULE_A_D_CONTENT_ID --other-content-id $SUBMODULE_A_F_CONTENT_ID --base-content-path "old_repo" --other-content-path "new_repo" --file-type git_submodule --copy-info move --context-lines 1 --inspect-lfs-pointers
  Unified diff output (is_binary: false):
  * (glob)
  rename from old_repo
  rename to new_repo
  --- a/old_repo
  +++ b/new_repo
  @@ -1 +1 @@
  -Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  +Subproject commit dddddddddddddddddddddddddddddddddddddddd
  === End of unified diff ===

test diff unified headerless method with actual content IDs
Test headerless diff between different versions of file1.txt
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of headerless diff ===

Test headerless diff between A and C (larger diff)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_C_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,6 +1,3 @@
  -content C
  +content A
   line 2
   line 3
  -line 4
  -line 5
  -line 6
  === End of headerless diff ===

Test headerless diff with custom context lines (1 line of context)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 1
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,2 +1,2 @@
  -content C
  +content A
   line 2
  @@ -9,4 +9,2 @@
   line 9
  -changed line 10
  -new line 11
  -new line 12
  +line 10
  === End of headerless diff ===

Test headerless diff with custom context lines (2 lines of context)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 2
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
  -content C
  +content A
   line 2
   line 3
  @@ -8,5 +8,3 @@
   line 8
   line 9
  -changed line 10
  -new line 11
  -new line 12
  +line 10
  === End of headerless diff ===

Test headerless diff with custom context lines (0 lines of context)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 0
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,1 +1,1 @@
  -content C
  +content A
  @@ -10,3 +10,1 @@
  -changed line 10
  -new line 11
  -new line 12
  +line 10
  === End of headerless diff ===

Test headerless diff with identical content (should have no diff output)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_A_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 0):
  === End of headerless diff ===

Test headerless diff with identical content from different contentIds (should have no diff output)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE2_A_CONTENT_ID --other-content-id $FILE2_B_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 0):
  === End of headerless diff ===

Test headerless diff error handling with non-existent content ID
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id 1111111111111111111111111111111111111111111111111111111111111111 --other-content-id $FILE1_A_CONTENT_ID
  Error: DiffService::diff_unified_headerless failed with ex(RequestError)
  * (glob)
  Caused by:
      RequestError { reason: diff_error(DiffError { reason: "Content not found: *" }) } (glob)
  [1]

Create binary file for testing binary diff handling
  $ testtool_drawdag -R repo <<'EOF'
  > D
  > # bookmark: D binary_test
  > # modify: D "binary_file.bin" "\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f"
  > EOF
  D=* (glob)

  $ BINARY_CONTENT_ID=$(mononoke_admin fetch -R repo -i $D -p binary_file.bin -k content-manifest | grep -a "Content-Id:" | cut -d' ' -f2)

Test diff-unified with text vs binary (text base, binary other)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $BINARY_CONTENT_ID --base-content-path "file1.txt" --other-content-path "binary_file.bin"
  Unified diff output (is_binary: true):
  diff --git a/file1.txt b/binary_file.bin
  Binary files a/file1.txt and b/binary_file.bin differ
  === End of unified diff ===

Test diff-unified with binary vs text (binary base, text other)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $BINARY_CONTENT_ID --other-content-id $FILE1_A_CONTENT_ID --base-content-path "binary_file.bin" --other-content-path "file1.txt"
  Unified diff output (is_binary: true):
  diff --git a/binary_file.bin b/file1.txt
  Binary files a/binary_file.bin and b/file1.txt differ
  === End of unified diff ===

Test diff-unified with binary vs binary (both binary, same content)
  $ diff_service_client diff-unified --repo-name repo --base-content-id $BINARY_CONTENT_ID --other-content-id $BINARY_CONTENT_ID --base-content-path "binary_file.bin" --other-content-path "binary_file.bin"
  Unified diff output (is_binary: true):
  === End of unified diff ===

Test headerless diff with binary files (default behavior - should show "Binary files differ")
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $BINARY_CONTENT_ID
  Headerless diff output (is_binary: true, num_chunks: 1):
  Binary files differ
  === End of headerless diff ===

Test headerless diff with binary files and inspect_binary_data flag
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $BINARY_CONTENT_ID --inspect-binary-data
  Headerless diff output (is_binary: true, num_chunks: 1):
  * (glob)
  === End of headerless diff ===


Test headerless diff with all options combined
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_C_CONTENT_ID --context-lines 1 --inspect-binary-data
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,6 +1,3 @@
  -content C
  +content A
   line 2
   line 3
  -line 4
  -line 5
  -line 6
  === End of headerless diff ===

Test headerless diff with changeset input
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of headerless diff ===

Test headerless diff with changeset input (larger diff)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $C --other-path file1.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,6 +1,3 @@
  -content C
  +content A
   line 2
   line 3
  -line 4
  -line 5
  -line 6
  === End of headerless diff ===

Test headerless diff with mixed input types - content ID vs changeset+path
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-changeset-id $B --other-path file1.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of headerless diff ===

Test headerless diff with mixed input types - changeset+path vs content ID
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id $A --base-path file1.txt --other-content-id $FILE1_C_CONTENT_ID
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,6 +1,3 @@
  -content C
  +content A
   line 2
   line 3
  -line 4
  -line 5
  -line 6
  === End of headerless diff ===

Test headerless diff with mixed input types and custom context lines
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-changeset-id $C --other-path file3.txt --context-lines 1
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,2 +1,2 @@
  -content C
  +content A
   line 2
  @@ -9,4 +9,2 @@
   line 9
  -changed line 10
  -new line 11
  -new line 12
  +line 10
  === End of headerless diff ===

Test headerless diff with mixed input types and inspect_binary_data flag
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id $A --base-path file1.txt --other-content-id $FILE1_B_CONTENT_ID --inspect-binary-data
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of headerless diff ===

Test headerless diff error handling with invalid arguments
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID
  Error: Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string
  [1]

Test headerless diff error handling with missing changeset path
  $ diff_service_client diff-unified-headerless --repo-name repo --base-commit-id $A --other-content-id $FILE1_B_CONTENT_ID
  Error: Must provide either --base-content-id, (--base-commit-id and --base-path), or --base-string
  [1]

Test headerless diff for file creation - file exists only in other changeset (created_file.txt was created in H)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id $C --base-path created_file.txt --other-changeset-id $H --other-path created_file.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +0,0 @@
  -This is a newly created file
  -with multiple lines
  -for testing
  === End of headerless diff ===

Test headerless diff for file deletion - file exists only in base changeset (file2.txt was deleted in I)
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id $H --base-path file2.txt --other-changeset-id $I --other-path file2.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -0,0 +1,1 @@
  +original content
  === End of headerless diff ===

Test streamed headerless diff with omitted options (sends options: None — exercises
the server's default-options path: defaults::CONTEXT_LINES, ignore_whitespace=false).
Mirrors the unary --omit-options case below; this is the streamed handler's
options: None server path, reachable from the CLI now that diff-unified-headerless
shares HeaderlessDiffArgs with the unary command.
  $ diff_service_client diff-unified-headerless --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --omit-options
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of headerless diff ===

Unary unified diff tests
=====
The unary handler is byte-equivalent to the streamed handler — same fixtures
should produce the same `content` bytes via a single response message instead
of a stream of `DiffUnifiedChunk`s.

Test unary unified diff with content IDs (modified file)
  $ diff_service_client diff-unified-unary --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt"
  Unary unified diff output (is_binary: false):
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unary unified diff ===

Test unary unified diff with changeset input
  $ diff_service_client diff-unified-unary --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt
  Unary unified diff output (is_binary: false):
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,3 +1,3 @@
  -content A
  +content B
   line 2
   line 3
  === End of unary unified diff ===

Test unary unified diff for file creation
  $ diff_service_client diff-unified-unary --repo-name repo --base-changeset-id $C --base-path created_file.txt --other-changeset-id $H --other-path created_file.txt
  Unary unified diff output (is_binary: false):
  diff --git a/created_file.txt b/created_file.txt
  new file mode 100644
  --- /dev/null
  +++ b/created_file.txt
  @@ -0,0 +1,3 @@
  +This is a newly created file
  +with multiple lines
  +for testing
  === End of unary unified diff ===

Test unary unified diff for file deletion
  $ diff_service_client diff-unified-unary --repo-name repo --base-changeset-id $H --base-path file2.txt --other-changeset-id $I --other-path file2.txt
  Unary unified diff output (is_binary: false):
  diff --git a/file2.txt b/file2.txt
  deleted file mode 100644
  --- a/file2.txt
  +++ /dev/null
  @@ -1,1 +0,0 @@
  -original content
  === End of unary unified diff ===

Test unary unified diff with diff_mode omit_content (placeholder)
  $ diff_service_client diff-unified-unary --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --base-content-path "file1.txt" --other-content-path "file1.txt" --diff-mode omit_content
  Unary unified diff output (is_binary: false):
  diff --git a/file1.txt b/file1.txt
  Binary file file1.txt has changed
  === End of unary unified diff ===

Test unary unified diff with --ignore-whitespace
  $ diff_service_client diff-unified-unary --repo-name repo --base-string "hello world" --other-string "hello  world" --ignore-whitespace
  Unary unified diff output (is_binary: false):
  
  === End of unary unified diff ===

Test unary unified diff error handling with empty inputs
  $ diff_service_client diff-unified-unary --repo-name repo --base-content-id $FILE1_A_CONTENT_ID
  Error: Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string
  [1]

Unary headerless diff tests
=====
The unary headerless handler is byte-equivalent to the streamed
diff_unified_headerless — same input fixtures, same body bytes, single
response message instead of streamed chunks.

Test unary headerless diff with content IDs (modified file)
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID
  Unary headerless diff output (is_binary: false):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of unary headerless diff ===

Test unary headerless diff with changeset input
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt
  Unary headerless diff output (is_binary: false):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of unary headerless diff ===

Test unary headerless diff with custom context_lines (1 line)
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-content-id $FILE3_C_CONTENT_ID --context-lines 1
  Unary headerless diff output (is_binary: false):
  @@ -1,2 +1,2 @@
  -content C
  +content A
   line 2
  @@ -9,4 +9,2 @@
   line 9
  -changed line 10
  -new line 11
  -new line 12
  +line 10
  === End of unary headerless diff ===

Test unary headerless diff for file creation (file exists only in other changeset)
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-changeset-id $C --base-path created_file.txt --other-changeset-id $H --other-path created_file.txt
  Unary headerless diff output (is_binary: false):
  @@ -1,3 +0,0 @@
  -This is a newly created file
  -with multiple lines
  -for testing
  === End of unary headerless diff ===

Test unary headerless diff for file deletion (file exists only in base changeset)
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-changeset-id $H --base-path file2.txt --other-changeset-id $I --other-path file2.txt
  Unary headerless diff output (is_binary: false):
  @@ -0,0 +1,1 @@
  +original content
  === End of unary headerless diff ===

Test unary headerless diff with --ignore-whitespace
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-string "hello world" --other-string "hello  world" --ignore-whitespace
  Unary headerless diff output (is_binary: false):
  
  === End of unary headerless diff ===

Test unary headerless diff with omitted options (sends options: None — exercises
the server's default-options path: defaults::CONTEXT_LINES, ignore_whitespace=false)
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_B_CONTENT_ID --omit-options
  Unary headerless diff output (is_binary: false):
  @@ -1,3 +1,3 @@
  -content B
  +content A
   line 2
   line 3
  === End of unary headerless diff ===

Test unary headerless diff error handling with missing input
  $ diff_service_client diff-unified-headerless-unary --repo-name repo --base-content-id $FILE1_A_CONTENT_ID
  Error: Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string
  [1]
