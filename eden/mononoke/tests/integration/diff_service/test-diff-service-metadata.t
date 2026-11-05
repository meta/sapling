# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

setup
  $ . "${TEST_FIXTURES}/library.sh"

Use a multiplexed store without ephemeral storage for ordinary commits.
  $ export MULTIPLEXED=1
  $ REPONAME="repo" setup_common_config
  $ REPOID=1 REPONAME=lfs_repo GIT_LFS_INTERPRET_POINTERS=1 setup_common_config
setup basic repo with some commits and files
  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C
  > # bookmark: C main
  > # modify: A "file1.txt" "content A\nline 2\nline 3\n"
  > # modify: B "file1.txt" "content B\nline 2\nline 3\n"
  > # modify: C "file1.txt" "content C\nline 2\nline 3\nline 4\nline 5\nline 6\n"
  > # modify: A "file3.txt" "content A\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n"
  > # modify: B "file3.txt" "content B\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n"
  > # modify: C "file3.txt" "content C\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nchanged line 10\nnew line 11\nnew line 12\n"
  > # modify: A "file2.txt" "original content\n"
  > # modify: B "file2.txt" "original content\n"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

A missing ordinary commit reports a missing commit even without ephemeral storage.
  $ MISSING_COMMIT=$(python3 -c "print('f' * 64)")
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id "$MISSING_COMMIT" --base-path file1.txt --other-changeset-id "$A" --other-path file1.txt > "$TESTTMP/missing-commit.out" 2> "$TESTTMP/missing-commit.err"
  [1]
  $ python3 -c "from pathlib import Path; assert 'commit not found' in Path('$TESTTMP/missing-commit.err').read_text()"

test metadata diff method with actual content IDs
Get content IDs for the files we created
  $ FILE1_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_B_CONTENT_ID=$(mononoke_admin fetch -R repo -i $B -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_C_CONTENT_ID=$(mononoke_admin fetch -R repo -i $C -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE2_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file2.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE2_B_CONTENT_ID=$(mononoke_admin fetch -R repo -i $B -p file2.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE3_A_CONTENT_ID=$(mononoke_admin fetch -R repo -i $A -p file3.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE3_C_CONTENT_ID=$(mononoke_admin fetch -R repo -i $C -p file3.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)


Test metadata diff with identical content (should have no changes)
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $FILE1_A_CONTENT_ID
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

Test metadata diff with identical content from different contentIds (should have no changes)
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE2_A_CONTENT_ID --other-content-id $FILE2_B_CONTENT_ID
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

Create binary file for testing binary diff handling
  $ testtool_drawdag -R repo <<'EOF'
  > D
  > # bookmark: D binary_test
  > # modify: D "binary_file.bin" "\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f"
  > EOF
  D=* (glob)

  $ BINARY_CONTENT_ID=$(mononoke_admin fetch -R repo -i $D -p binary_file.bin -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test metadata diff with binary files will not show line count
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $BINARY_CONTENT_ID --other-content-id $BINARY_CONTENT_ID
  Metadata diff output:
  Base file info:
    Content type: BINARY
  Other file info:
    Content type: BINARY
  Lines count: Not available (non-text files)
  === End of metadata diff ===


Test metadata diff with a text and a binary file. It assumes the text file has been deleted.
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $BINARY_CONTENT_ID
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: BINARY
  Lines count:
    Added lines: 0
    Deleted lines: 3
    Significant added lines: 0
    Significant deleted lines: 3
  === End of metadata diff ===

Test metadata diff with changeset input
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $B --other-path file1.txt
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

Test metadata diff with changeset input - file creation
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path nonexistent.txt --other-changeset-id $A --other-path file1.txt
  Metadata diff output:
  Base file info:
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 3
    Deleted lines: 0
    Significant added lines: 3
    Significant deleted lines: 0
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with changeset input - file deletion
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $A --other-path nonexistent.txt
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
  Lines count:
    Added lines: 0
    Deleted lines: 3
    Significant added lines: 0
    Significant deleted lines: 3
  === End of metadata diff ===

Test metadata diff error handling with non-existent content ID
  $ diff_service_client metadata-diff --repo-name repo --base-content-id 1111111111111111111111111111111111111111111111111111111111111111 --other-content-id $FILE1_A_CONTENT_ID
  Error: DiffService::metadata_diff failed with ex(RequestError)
  * (glob)
  Caused by:
      RequestError { reason: diff_error(DiffError { reason: "Content not found: *" }) } (glob)
  [1]

Test metadata diff with mixed input types - content ID vs changeset+path
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-changeset-id $B --other-path file1.txt
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
    Deleted lines: 1
    Significant added lines: 1
    Significant deleted lines: 1
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with mixed input types - changeset+path vs content ID
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-content-id $FILE1_C_CONTENT_ID
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 4
    Deleted lines: 1
    Significant added lines: 4
    Significant deleted lines: 1
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with mixed input types - larger file
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE3_A_CONTENT_ID --other-changeset-id $C --other-path file3.txt
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 4
    Deleted lines: 2
    Significant added lines: 4
    Significant deleted lines: 2
    First added line number: 1
  === End of metadata diff ===

Test metadata diff with mixed input types - binary file
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-content-id $BINARY_CONTENT_ID
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: BINARY
  Lines count:
    Added lines: 0
    Deleted lines: 3
    Significant added lines: 0
    Significant deleted lines: 3
  === End of metadata diff ===

Test metadata diff error handling with invalid arguments
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID
  Error: Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string
  [1]

Test metadata diff error handling with missing changeset path
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --other-content-id $FILE1_B_CONTENT_ID
  Error: Must provide either --base-content-id, (--base-commit-id and --base-path), or --base-string
  [1]

Create files with generated content for testing generated file detection
  $ testtool_drawdag -R repo <<'EOF'
  > H-I-J
  > # bookmark: J generated_test
  > # modify: H "generated_file.txt" "@generated\n// This file is generated\nline 1\nline 2\nline 3\n"
  > # modify: I "generated_file.txt" "@generated\n// This file is generated\nline 1\nline 2\nline 3\nline 4\n"
  > # modify: J "partially_generated.txt" "@partially-generated\n// This file is partially generated\n// BEGIN MANUAL SECTION\nmanual line 1\nmanual line 2\n// END MANUAL SECTION\ngenerated line 1\ngenerated line 2\n"
  > EOF
  H=* (glob)
  I=* (glob)
  J=* (glob)

Get content IDs for generated files
  $ GENERATED_H_CONTENT_ID=$(mononoke_admin fetch -R repo -i $H -p generated_file.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ GENERATED_I_CONTENT_ID=$(mononoke_admin fetch -R repo -i $I -p generated_file.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ PARTIALLY_GENERATED_CONTENT_ID=$(mononoke_admin fetch -R repo -i $J -p partially_generated.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test metadata diff with fully generated files
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $H --base-path generated_file.txt --other-changeset-id $I --other-path generated_file.txt
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: FULLY
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: FULLY
  Lines count:
    Added lines: 1
    Deleted lines: 0
    Significant added lines: 0
    Significant deleted lines: 0
    First added line number: 6
  === End of metadata diff ===

Test metadata diff with partially generated file
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $J --other-path partially_generated.txt
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: PARTIALLY
  Lines count:
    Added lines: 8
    Deleted lines: 3
    Significant added lines: 2
    Significant deleted lines: 3
    First added line number: 1
  === End of metadata diff ===

Test metadata diff between generated and non-generated files
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $GENERATED_H_CONTENT_ID
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: TEXT
    Generated status: FULLY
  Lines count:
    Added lines: 3
    Deleted lines: 1
    Significant added lines: 0
    Significant deleted lines: 1
    First added line number: 1
  === End of metadata diff ===

Setup repository with Git submodules
  $ testtool_drawdag -R repo <<'EOF'
  > E-F-G
  > # bookmark: G submodule_main
  > # modify: E "regular_file.txt" "regular content\n"
  > # modify: E "submodules/repo_a" git-submodule &aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  > # modify: F "submodules/repo_a" git-submodule &bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  > # modify: F "submodules/repo_b" git-submodule &cccccccccccccccccccccccccccccccccccccccc
  > # delete: G "submodules/repo_b"
  > # modify: G "submodules/repo_c" git-submodule &eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
  > EOF
  E=* (glob)
  F=* (glob)
  G=* (glob)

Test metadata diff with Git submodules
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $E --base-path submodules/repo_a --other-changeset-id $F --other-path submodules/repo_a
  Metadata diff output:
  Base file info:
    File type: GIT_SUBMODULE
  Other file info:
    File type: GIT_SUBMODULE
  Lines count: Not available (non-text files)
  === End of metadata diff ===

Test metadata diff with mixed file types (regular vs submodule)
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $E --base-path regular_file.txt --other-changeset-id $E --other-path submodules/repo_a
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: GIT_SUBMODULE
  Lines count: Not available (non-text files)
  === End of metadata diff ===

Create executable file for testing file type detection
  $ testtool_drawdag -R repo <<'EOF'
  > K
  > # bookmark: K executable_test
  > # modify: K "executable_file.sh" executable "#!/bin/bash\necho 'Hello World'\n"
  > EOF
  K=* (glob)

Test metadata diff with executable files
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $K --other-path executable_file.sh
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: EXECUTABLE
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 2
    Deleted lines: 3
    Significant added lines: 2
    Significant deleted lines: 3
    First added line number: 1
  === End of metadata diff ===

Create symlink for testing file type detection
  $ testtool_drawdag -R repo <<'EOF'
  > L
  > # bookmark: L symlink_test
  > # modify: L "symlink_file" symlink "target_file.txt"
  > EOF
  L=* (glob)

Test metadata diff with symlinks
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id $A --base-path file1.txt --other-changeset-id $L --other-path symlink_file
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: SYMLINK
    Content type: TEXT
    Generated status: NON_GENERATED
  Lines count:
    Added lines: 1
    Deleted lines: 3
    Significant added lines: 1
    Significant deleted lines: 3
    First added line number: 1
  === End of metadata diff ===

Create non-UTF8 file for testing content type detection
  $ testtool_drawdag -R repo <<'EOF'
  > M
  > # bookmark: M non_utf8_test
  > # modify: M "non_utf8_file.txt" "\xff\xfe\x00\x48\x00\x65\x00\x6c\x00\x6c\x00\x6f"
  > EOF
  M=* (glob)

  $ NON_UTF8_CONTENT_ID=$(mononoke_admin fetch -R repo -i $M -p non_utf8_file.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test metadata diff with non-UTF8 files
  $ diff_service_client metadata-diff --repo-name repo --base-content-id $FILE1_A_CONTENT_ID --other-content-id $NON_UTF8_CONTENT_ID
  Metadata diff output:
  Base file info:
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    Content type: BINARY
  Lines count:
    Added lines: 0
    Deleted lines: 3
    Significant added lines: 0
    Significant deleted lines: 3
  === End of metadata diff ===

LFS Pointer Tests
=====

Create LFS files in the LFS-enabled repo
  $ testtool_drawdag -R lfs_repo <<'EOF'
  > P-Q-R
  > # bookmark: R lfs_main
  > # modify: P "lfs_file.bin" regular lfs_pointer "large file content base"
  > # modify: P "regular_file.txt" "regular text content\nline 2\nline 3\n"
  > # modify: Q "lfs_file.bin" regular lfs_pointer "large file content modified"
  > # modify: R "new_lfs_file.bin" regular lfs_pointer "another large file content"
  > EOF
  P=* (glob)
  Q=* (glob)
  R=* (glob)

Test metadata diff with LFS files via changeset path - both sides LFS
  $ diff_service_client metadata-diff --repo-name lfs_repo --base-changeset-id $P --base-path lfs_file.bin --other-changeset-id $Q --other-path lfs_file.bin
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: LFS_POINTER
  Other file info:
    File type: REGULAR
    Content type: LFS_POINTER
  Lines count: Not available (non-text files)
  === End of metadata diff ===

Test metadata diff with LFS file creation
  $ diff_service_client metadata-diff --repo-name lfs_repo --base-changeset-id $Q --base-path new_lfs_file.bin --other-changeset-id $R --other-path new_lfs_file.bin
  Metadata diff output:
  Base file info:
  Other file info:
    File type: REGULAR
    Content type: LFS_POINTER
  Lines count: Not available (non-text files)
  === End of metadata diff ===

Test metadata diff with LFS vs regular file (different content types)
  $ diff_service_client metadata-diff --repo-name lfs_repo --base-changeset-id $P --base-path regular_file.txt --other-changeset-id $P --other-path lfs_file.bin
  Metadata diff output:
  Base file info:
    File type: REGULAR
    Content type: TEXT
    Generated status: NON_GENERATED
  Other file info:
    File type: REGULAR
    Content type: LFS_POINTER
  Lines count:
    Added lines: 0
    Deleted lines: 3
    Significant added lines: 0
    Significant deleted lines: 3
  === End of metadata diff ===

Test metadata diff with LFS content IDs (without LFS metadata, falls through to content analysis)
  $ LFS_P_CONTENT_ID=$(mononoke_admin fetch -R lfs_repo -i $P -p lfs_file.bin -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ LFS_Q_CONTENT_ID=$(mononoke_admin fetch -R lfs_repo -i $Q -p lfs_file.bin -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Content IDs without changeset context have no LFS metadata, so content type depends on the stored content
  $ diff_service_client metadata-diff --repo-name lfs_repo --base-content-id $LFS_P_CONTENT_ID --other-content-id $LFS_Q_CONTENT_ID
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
    First added line number: 1
  === End of metadata diff ===
