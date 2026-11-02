# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ setup_common_config
  $ setup_configerator_configs
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --no-default-files --print-hg-hashes <<EOF
  > C
  > |
  > B
  > |
  > A
  > # modify: A file1.txt "line1\nline2\nline3\nline4\nline5\n"
  > # modify: B file1.txt "line1\nmodified_line2\nline3\nline4\nline5\n"
  > # modify: B file2.txt "new\nfile\ncontent\n"
  > # modify: C file1.txt "line1\nmodified_line2\nline3\nline4\nline5\nextra_line\n"
  > # modify: C binary_file.bin "\x00binary\x01data\x02here"
  > EOF
  A=0e65dc3f5847a18b846714030f3655f56a734ad5
  B=da35c8e2969c03f7f73c82113c6059a8ccc056ab
  C=33c98fbf8f9f37589634b802f5452a5a4a8bf01a

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

Let's debug what scsc info actually returns:
  $ scsc info --repo repo --hg-commit-id "$A" --path file1.txt --json
  * (glob)

  $ scsc info --repo repo --hg-commit-id "$A" --path file1.txt
  Path: file1.txt
  Type: file
  Id: * (glob)
  Content-SHA1: * (glob)
  Content-SHA256: * (glob)
  Size: 30 bytes

Now let's try to get file IDs correctly from the Id field:
  $ FILE1_A_ID=$(scsc info --repo repo --hg-commit-id "$A" --path file1.txt --json | jq -r '.id // empty')
  $ FILE1_B_ID=$(scsc info --repo repo --hg-commit-id "$B" --path file1.txt --json | jq -r '.id // empty')
  $ FILE1_C_ID=$(scsc info --repo repo --hg-commit-id "$C" --path file1.txt --json | jq -r '.id // empty')
  $ FILE2_B_ID=$(scsc info --repo repo --hg-commit-id "$B" --path file2.txt --json | jq -r '.id // empty')
  $ BINARY_C_ID=$(scsc info --repo repo --hg-commit-id "$C" --path binary_file.bin --json | jq -r '.id // empty')

Let's see what these file IDs actually look like:
  $ echo "FILE1_A_ID: $FILE1_A_ID"
  FILE1_A_ID: * (glob)
  $ echo "FILE1_B_ID: $FILE1_B_ID"
  FILE1_B_ID: * (glob)

Test basic file-diff functionality:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  @@ -1,5 +1,5 @@
   line1
  -modified_line2
  +line2
   line3
   line4
   line5

Test headerless diff with different context lines:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" -U 1 by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  @@ -1,3 +1,3 @@
   line1
  -modified_line2
  +line2
   line3

Test headerless diff with same file (should be empty):
  $ scsc file-diff --other-file-id "$FILE1_A_ID" by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt

Test headerless diff between different files:
  $ scsc file-diff --other-file-id "$FILE2_B_ID" by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  @@ -1,3 +1,5 @@
  -new
  -file
  -content
  +line1
  +line2
  +line3
  +line4
  +line5

Test with binary file (should show "Binary files differ"):
  $ scsc file-diff --other-file-id "$BINARY_C_ID" by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  Binary files differ

Test addition: B -> C change (extra_line addition):
  $ scsc file-diff --other-file-id "$FILE1_C_ID" by-commit-path --repo repo --hg-commit-id "$B" --path file1.txt
  @@ -3,4 +3,3 @@
   line3
   line4
   line5
  -extra_line

Test reverse diff: C -> B (extra_line removal):
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-commit-path --repo repo --hg-commit-id "$C" --path file1.txt
  @@ -3,3 +3,4 @@
   line3
   line4
   line5
  +extra_line

Test A -> C (shows both line modification and extra_line addition):
  $ scsc file-diff --other-file-id "$FILE1_C_ID" by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  @@ -1,6 +1,5 @@
   line1
  -modified_line2
  +line2
   line3
   line4
   line5
  -extra_line

Test with zero context lines:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" -U 0 by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  @@ -2,1 +2,1 @@
  -modified_line2
  +line2

Test with maximum context (entire file):
  $ scsc file-diff --other-file-id "$FILE1_B_ID" -U 10 by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  @@ -1,5 +1,5 @@
   line1
  -modified_line2
  +line2
   line3
   line4
   line5

Test error case: Non-existent file ID:
  $ scsc file-diff --other-file-id "invalid_file_id" by-commit-path --repo repo --hg-commit-id "$A" --path file1.txt
  *scsc: error: File ID must have even number of characters, got: invalid_file_id (glob)
  [1]

Test error case: Non-existent path:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-commit-path --repo repo --hg-commit-id "$A" --path nonexistent.txt
  *scsc: error: SourceControlService::file_diff failed with request_error(RequestError): RequestError: failed to resolve target file: file not found * (glob)
  [1]

Test binary file with same binary file:
  $ scsc file-diff --other-file-id "$BINARY_C_ID" by-commit-path --repo repo --hg-commit-id "$C" --path binary_file.bin
  Binary files differ

Get SHA-1 and SHA-256 content hashes for testing:
  $ FILE1_A_SHA1=$(scsc info --repo repo --hg-commit-id "$A" --path file1.txt --json | jq -r '.content_sha1 // empty')
  $ FILE1_B_SHA1=$(scsc info --repo repo --hg-commit-id "$B" --path file1.txt --json | jq -r '.content_sha1 // empty')
  $ FILE1_A_SHA256=$(scsc info --repo repo --hg-commit-id "$A" --path file1.txt --json | jq -r '.content_sha256 // empty')
  $ FILE1_B_SHA256=$(scsc info --repo repo --hg-commit-id "$B" --path file1.txt --json | jq -r '.content_sha256 // empty')

Test with by-id specifier:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-id --repo repo --id "$FILE1_A_ID"
  @@ -1,5 +1,5 @@
   line1
  -modified_line2
  +line2
   line3
   line4
   line5

Test with by-sha1-hash specifier:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-sha1-hash --repo repo --sha1 "$FILE1_A_SHA1"
  @@ -1,5 +1,5 @@
   line1
  -modified_line2
  +line2
   line3
   line4
   line5

Test with by-sha256-hash specifier:
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-sha256-hash --repo repo --sha256 "$FILE1_A_SHA256"
  @@ -1,5 +1,5 @@
   line1
  -modified_line2
  +line2
   line3
   line4
   line5
