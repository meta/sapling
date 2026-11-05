# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for SCS with remote diff service
# Tests that SCS correctly routes diff operations to the diff_service

setup
  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

  $ INFINITEPUSH_ALLOW_WRITES=true REPONAME="repo" setup_common_config

Allow proxy service to access the repo
  $ cat >> "$TESTTMP/mononoke-config/common/common.toml" << CONFIG
  > [[global_allowlist]]
  > identity_type = "SERVICE_IDENTITY"
  > identity_data = "proxy"
  > CONFIG

Enable hg snapshot extension for creating snapshots
  $ cd "$TESTTMP"
  $ cat >> "$HGRCPATH" <<EOF
  > [extensions]
  > snapshot =
  > commitcloud =
  > amend =
  > EOF

Create a test repository with files to diff. E adds a binary file and
deletes file2.txt to give us the removed-file and binary-file diff shapes.
F adds an executable and a symlink.
  $ testtool_drawdag -R repo --print-hg-hashes <<'EOF'
  > A-B-C-D-E-F
  > # bookmark: D main
  > # modify: A "file1.txt" "line 1\nline 2\nline 3\nline 4\nline 5\n"
  > # modify: B "file1.txt" "line 1\nmodified line 2\nline 3\nline 4\nline 5\n"
  > # modify: C "file1.txt" "line 1\nmodified line 2\nline 3\nmodified line 4\nline 5\nline 6\n"
  > # modify: A "file2.txt" "original\ncontent\nhere\n"
  > # modify: B "file2.txt" "updated\ncontent\nhere\n"
  > # copy: D "copied.txt" "line 1\nmodified line 2\nline 3\nmodified line 4\nline 5\nline 6\n" C "file1.txt"
  > # delete: E "file2.txt"
  > # modify: E "binary.bin" "\x00binary\x01data\x02here"
  > # modify: F "script.sh" executable "echo hello\n"
  > # modify: F "link" symlink "file1.txt"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)
  E=* (glob)
  F=* (glob)

Convert HG hashes to bonsai for scsc commands
  $ A_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $A 2>/dev/null)
  $ B_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $B 2>/dev/null)
  $ C_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $C 2>/dev/null)
  $ D_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $D 2>/dev/null)
  $ E_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $E 2>/dev/null)
  $ F_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $F 2>/dev/null)

Start mononoke for snapshot operations
  $ mononoke
  $ wait_for_mononoke

Clone repo and create a snapshot with local changes
  $ hg clone -q mono:repo client1
  $ cd client1
  $ hg checkout $D -q

Create local changes for the snapshot
  $ echo "snapshot line 1" > file1.txt
  $ echo "snapshot line 2" >> file1.txt

Create the snapshot (creates ephemeral bubble with ID 1)
  $ SNAPSHOT_ID=$(HGPLAIN=1 hg snapshot create)
  $ echo "Created snapshot: $SNAPSHOT_ID"
  Created snapshot: * (glob)
  $ cd ..

Start diff service in background
  $ start_and_wait_for_diff_service

Configure SCS to use local diff service via host:port
  $ cat >> "$TESTTMP/mononoke-config/repos/$REPONAME/server.toml" << CONFIG
  > [remote_diff_config]
  > host_port = "$(diff_service_address)"
  > CONFIG

Start SCS with remote diff enabled
  $ start_and_wait_for_scs_server --diff-remotely --scuba-log-file "$TESTTMP/scs_scuba.json"

Pin streamed transport for the existing fixtures. The end-of-test JK-flip
block re-runs a representative subset under the unary transport to assert
byte equivalence and that the new method shows up in the diff_service log.
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:remote_diff_unary": false
  >   }
  > }
  > EOF

== Test scsc file-diff (file content diffs by content ID) ==

Get file IDs for testing
  $ FILE1_A_ID=$(mononoke_admin fetch -R repo -i $A -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_B_ID=$(mononoke_admin fetch -R repo -i $B -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE1_C_ID=$(mononoke_admin fetch -R repo -i $C -p file1.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE2_A_ID=$(mononoke_admin fetch -R repo -i $A -p file2.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)
  $ FILE2_B_ID=$(mononoke_admin fetch -R repo -i $B -p file2.txt -k content-manifest | grep "Content-Id:" | cut -d' ' -f2)

Test file-diff via SCS (this should route to diff service)
  $ scsc file-diff --other-file-id "$FILE1_A_ID" by-id --repo repo --id "$FILE1_B_ID"
  @@ -1,5 +1,5 @@
   line 1
  -line 2
  +modified line 2
   line 3
   line 4
   line 5

Test another diff to verify service is being used
  $ scsc file-diff --other-file-id "$FILE1_B_ID" by-id --repo repo --id "$FILE1_C_ID"
  @@ -1,5 +1,6 @@
   line 1
   modified line 2
   line 3
  -line 4
  +modified line 4
   line 5
  +line 6

Test with different files
  $ scsc file-diff --other-file-id "$FILE2_A_ID" by-id --repo repo --id "$FILE2_B_ID"
  @@ -1,3 +1,3 @@
  -original
  +updated
   content
   here

== Test scsc diff (commit-level diffs) ==

Test basic commit diff with remote service
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI"
  diff --git a/B b/B
  new file mode 100644
  --- /dev/null
  +++ b/B
  @@ -0,0 +1,1 @@
  +B
  \ No newline at end of file
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,5 +1,5 @@
   line 1
  -line 2
  +modified line 2
   line 3
   line 4
   line 5
  diff --git a/file2.txt b/file2.txt
  --- a/file2.txt
  +++ b/file2.txt
  @@ -1,3 +1,3 @@
  -original
  +updated
   content
   here

Test diff with modifications
  $ scsc diff --repo repo -i "$B_BONSAI" -i "$C_BONSAI"
  diff --git a/C b/C
  new file mode 100644
  --- /dev/null
  +++ b/C
  @@ -0,0 +1,1 @@
  +C
  \ No newline at end of file
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,5 +1,6 @@
   line 1
   modified line 2
   line 3
  -line 4
  +modified line 4
   line 5
  +line 6

Test paths-only mode with remote service
  $ scsc diff --repo repo --paths-only -i "$B_BONSAI" -i "$C_BONSAI"
  A C
  M file1.txt

Test with copy detection
  $ scsc diff --repo repo -i "$C_BONSAI" -i "$D_BONSAI"
  diff --git a/D b/D
  new file mode 100644
  --- /dev/null
  +++ b/D
  @@ -0,0 +1,1 @@
  +D
  \ No newline at end of file
  diff --git a/file1.txt b/copied.txt
  copy from file1.txt
  copy to copied.txt

Test metadata diff format with remote service
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI" -f metadata-diff
  --- /dev/null,
  +++ b/B, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  first added line number: 1, 1 significant lines (1 added, 0 deleted), 1 total (1 added, 0 deleted)
  
  --- a/file1.txt, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  +++ b/file1.txt, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  first added line number: 2, 2 significant lines (1 added, 1 deleted), 2 total (1 added, 1 deleted)
  
  --- a/file2.txt, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  +++ b/file2.txt, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  first added line number: 1, 2 significant lines (1 added, 1 deleted), 2 total (1 added, 1 deleted)
  

Test removed file (D -> E deletes file2.txt)
  $ scsc diff --repo repo -i "$D_BONSAI" -i "$E_BONSAI" --path file2.txt
  diff --git a/file2.txt b/file2.txt
  deleted file mode 100644
  --- a/file2.txt
  +++ /dev/null
  @@ -1,3 +0,0 @@
  -updated
  -content
  -here

Test binary file (D -> E adds binary.bin)
  $ scsc diff --repo repo -i "$D_BONSAI" -i "$E_BONSAI" --path binary.bin
  diff --git a/binary.bin b/binary.bin
  new file mode 100644
  Binary file binary.bin has changed

Test --placeholders-only (omit content)
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI" --path file1.txt --placeholders-only
  diff --git a/file1.txt b/file1.txt
  Binary file file1.txt has changed

Test multi-path explicit selection (multiple --path values)
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$C_BONSAI" --path file1.txt --path file2.txt --ordered
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,5 +1,6 @@
   line 1
  -line 2
  +modified line 2
   line 3
  -line 4
  +modified line 4
   line 5
  +line 6
  diff --git a/file2.txt b/file2.txt
  --- a/file2.txt
  +++ b/file2.txt
  @@ -1,3 +1,3 @@
  -original
  +updated
   content
   here

Test executable and symlink diffs (E -> F)
  $ scsc diff --repo repo -i "$E_BONSAI" -i "$F_BONSAI" --ordered
  diff --git a/F b/F
  new file mode 100644
  --- /dev/null
  +++ b/F
  @@ -0,0 +1,1 @@
  +F
  \ No newline at end of file
  diff --git a/link b/link
  new file mode 120000
  --- /dev/null
  +++ b/link
  @@ -0,0 +1,1 @@
  +file1.txt
  \ No newline at end of file
  diff --git a/script.sh b/script.sh
  new file mode 100755
  --- /dev/null
  +++ b/script.sh
  @@ -0,0 +1,1 @@
  +echo hello

== Test scsc diff with snapshot via remote diff service ==

Test paths-only diff between regular commit (D) and snapshot
  $ scsc diff --repo repo -i "$D_BONSAI" --snapshot-id "$SNAPSHOT_ID" --paths-only
  M file1.txt

Test full diff between regular commit (D) and snapshot
  $ scsc diff --repo repo -i "$D_BONSAI" --snapshot-id "$SNAPSHOT_ID"
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,6 +1,2 @@
  -line 1
  -modified line 2
  -line 3
  -modified line 4
  -line 5
  -line 6
  +snapshot line 1
  +snapshot line 2

Test metadata diff with snapshot via remote diff service
  $ scsc diff --repo repo -i "$D_BONSAI" --snapshot-id "$SNAPSHOT_ID" -f metadata-diff > "$TESTTMP/explicit-parent-metadata.out"
  $ cat "$TESTTMP/explicit-parent-metadata.out"
  --- a/file1.txt, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  +++ b/file1.txt, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  first added line number: 1, * significant lines (* added, * deleted), * total (* added, * deleted) (glob)
  

The default persistent parent can be read through the snapshot's bubble context.
  $ scsc diff --repo repo --snapshot-id "$SNAPSHOT_ID" -f metadata-diff > "$TESTTMP/implicit-parent-metadata.out"
  $ diff -u "$TESTTMP/explicit-parent-metadata.out" "$TESTTMP/implicit-parent-metadata.out"

== Verify diff service was called ==

Verify that the diff service was actually called by checking its logs
First check if the diff service log file exists and has content
  $ test -f "$TESTTMP/diff-service.json" && echo "Log file exists" || echo "Log file missing"
  Log file exists

Check what methods are actually being logged
  $ cat "$TESTTMP/diff-service.json" | jq -r '.normal.method' | grep -v null | sort | uniq
  commit_compare
  diff_unified
  diff_unified_headerless
  metadata_diff

The diff service should have logged successful operations
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete") | .normal.status' | sort | uniq
  SUCCESS

Verify we got multiple requests to the diff service (including snapshot diffs)
  $ COUNT=$(cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete")' | wc -l | sed 's/^[[:space:]]*//')
  $ test "$COUNT" -ge 8 && echo "Got $COUNT requests including snapshot diffs (OK)" || echo "Got $COUNT requests (expected >= 8)"
  Got * requests including snapshot diffs (OK) (glob)

== Verify client metadata is forwarded from SCS to diff service ==

The internal client forwards X-Client-Info metadata. The OSS plaintext client
has no authenticated request-header channel, so the field remains absent.
  $ CLIENT_ENTRY_POINT=$(jq -r 'select(.normal.log_tag == "Request complete") | .normal.client_entry_point' "$TESTTMP/diff-service.json" | sort | uniq)
  $ if [ -n "$FB_TEST_FIXTURES" ]; then test "$CLIENT_ENTRY_POINT" = scs; else test "$CLIENT_ENTRY_POINT" = null; fi

Verify request type is logged (proves the new add("type", "thrift") works)
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete") | .normal.type' | sort | uniq
  thrift

== Flip the unary JK and verify byte equivalence + new method in scuba ==

  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:remote_diff_unary": true
  >   }
  > }
  > EOF

The SCS server polls the JK config file every 1s; wait long enough for
the new value to be picked up by the in-memory cache before issuing the
first request that depends on it. Without this, the first scsc call
after the flip races the polling cycle and may still see the old value,
while later calls reliably use the new value.
  $ sleep 2

Re-run every scsc fixture from the streamed block above under the unary
transport. Outputs must be byte-identical to the matching streamed
invocation earlier in this file.

scsc file-diff (file content diffs by content ID)
  $ scsc file-diff --other-file-id "$FILE1_A_ID" by-id --repo repo --id "$FILE1_B_ID"
  @@ -1,5 +1,5 @@
   line 1
  -line 2
  +modified line 2
   line 3
   line 4
   line 5

scsc diff (commit-level diff)
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI"
  diff --git a/B b/B
  new file mode 100644
  --- /dev/null
  +++ b/B
  @@ -0,0 +1,1 @@
  +B
  \ No newline at end of file
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,5 +1,5 @@
   line 1
  -line 2
  +modified line 2
   line 3
   line 4
   line 5
  diff --git a/file2.txt b/file2.txt
  --- a/file2.txt
  +++ b/file2.txt
  @@ -1,3 +1,3 @@
  -original
  +updated
   content
   here

Removed file (D -> E deletes file2.txt)
  $ scsc diff --repo repo -i "$D_BONSAI" -i "$E_BONSAI" --path file2.txt
  diff --git a/file2.txt b/file2.txt
  deleted file mode 100644
  --- a/file2.txt
  +++ /dev/null
  @@ -1,3 +0,0 @@
  -updated
  -content
  -here

Binary file (D -> E adds binary.bin)
  $ scsc diff --repo repo -i "$D_BONSAI" -i "$E_BONSAI" --path binary.bin
  diff --git a/binary.bin b/binary.bin
  new file mode 100644
  Binary file binary.bin has changed

--placeholders-only (omit content)
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI" --path file1.txt --placeholders-only
  diff --git a/file1.txt b/file1.txt
  Binary file file1.txt has changed

Multi-path explicit selection (multiple --path values)
  $ scsc diff --repo repo -i "$A_BONSAI" -i "$C_BONSAI" --path file1.txt --path file2.txt --ordered
  diff --git a/file1.txt b/file1.txt
  --- a/file1.txt
  +++ b/file1.txt
  @@ -1,5 +1,6 @@
   line 1
  -line 2
  +modified line 2
   line 3
  -line 4
  +modified line 4
   line 5
  +line 6
  diff --git a/file2.txt b/file2.txt
  --- a/file2.txt
  +++ b/file2.txt
  @@ -1,3 +1,3 @@
  -original
  +updated
   content
   here

After the JK flip, the unary methods appear in the diff_service log
alongside the streamed methods from the earlier blocks.
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete") | .normal.method' | sort | uniq
  commit_compare
  diff_unified
  diff_unified_headerless
  diff_unified_headerless_unary
  diff_unified_unary
  metadata_diff

Verify the A/B split machinery: scuba rows logged for the unary methods
include scm/mononoke:remote_diff_unary in enabled_experiments_jk. This
proves the routing decision and the per-request scuba tag agree by
construction — both use correlator-based consistent hashing on the same
JK, so a Scuba split on this column corresponds 1:1 to the transport
that actually served the request.
  $ UNARY_JK=$(jq -r 'select(.normal.method == "diff_unified_unary") | .normvector.enabled_experiments_jk[]?' "$TESTTMP/diff-service.json")
  $ if [ -n "$FB_TEST_FIXTURES" ]; then echo "$UNARY_JK" | grep -q "^scm/mononoke:remote_diff_unary"; else test -z "$UNARY_JK"; fi

Unified diffs name files by content ID, so the diff service does not load
the changeset. The snapshot side keeps its commit path because the diff
service opens the snapshot's bubble from the changeset.
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Computed unified diff" or .normal.log_tag == "Computed unified diff (unary)") | "base:\(if .normal.commit then "commit" elif .normal.base_content_id then "content" else "none" end) other:\(if .normal.other_commit then "commit" elif .normal.other_content_id then "content" else "none" end)"' | sort -u
  base:content other:commit
  base:content other:content
  base:content other:none
  base:none other:content

== Test remote file-level diff failures are not retried locally ==

Given commit comparison is local so `scsc diff` reaches the remote file-level diff methods.
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:remote_commit_compare": false
  >   }
  > }
  > EOF
  $ sleep 2

Given remote file-level diffs are enabled and the diff service becomes unavailable.
  $ kill "$DIFF_SERVICE_PID" 2>/dev/null && wait "$DIFF_SERVICE_PID" 2>/dev/null; true

When SCS receives a file-diff request.
  $ if scsc file-diff --other-file-id "$FILE1_A_ID" by-id --repo repo --id "$FILE1_B_ID" > "$TESTTMP/no_file_fallback.out" 2> "$TESTTMP/no_file_fallback.err"; then echo "unexpected success"; cat "$TESTTMP/no_file_fallback.out"; else echo "remote file diff failure surfaced"; fi
  remote file diff failure surfaced

Then SCS returns its standard internal error for the unavailable remote service.
  $ grep -q 'SourceControlService::file_diff failed with internal_error(InternalError)' "$TESTTMP/no_file_fallback.err" && echo "SCS internal error returned"
  SCS internal error returned

Then SCS does not produce the local file-diff result.
  $ test ! -s "$TESTTMP/no_file_fallback.out" && echo "no local file diff output"
  no local file diff output
The JustKnob above keeps commit_compare local internally, so the failure
surfaces from commit_file_diffs. OSS has no JustKnobs and routes remotely
whenever an address is configured, so commit_compare is the first call to fail.
  $ if [ -n "$FB_TEST_FIXTURES" ]; then FAILING_METHOD=commit_file_diffs; else FAILING_METHOD=commit_compare; fi

When SCS receives a commit-file raw diff request.
  $ if scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/no_raw_fallback.out" 2> "$TESTTMP/no_raw_fallback.err"; then echo "unexpected success"; cat "$TESTTMP/no_raw_fallback.out"; else echo "remote raw diff failure surfaced"; fi
  remote raw diff failure surfaced

Then SCS returns its standard internal error for the unavailable remote service.
  $ grep -q "SourceControlService::$FAILING_METHOD failed with internal_error(InternalError)" "$TESTTMP/no_raw_fallback.err" && echo "SCS internal error returned"
  SCS internal error returned

Then SCS does not produce the local raw diff result.
  $ test ! -s "$TESTTMP/no_raw_fallback.out" && echo "no local raw diff output"
  no local raw diff output
When SCS receives a commit-file metadata diff request.
  $ if scsc diff --repo repo -i "$A_BONSAI" -i "$B_BONSAI" -f metadata-diff > "$TESTTMP/no_metadata_fallback.out" 2> "$TESTTMP/no_metadata_fallback.err"; then echo "unexpected success"; cat "$TESTTMP/no_metadata_fallback.out"; else echo "remote metadata diff failure surfaced"; fi
  remote metadata diff failure surfaced

Then SCS returns its standard internal error for the unavailable remote service.
  $ grep -q "SourceControlService::$FAILING_METHOD failed with internal_error(InternalError)" "$TESTTMP/no_metadata_fallback.err" && echo "SCS internal error returned"
  SCS internal error returned

Then SCS does not produce the local metadata diff result.
  $ test ! -s "$TESTTMP/no_metadata_fallback.out" && echo "no local metadata diff output"
  no local metadata diff output
Then SCS does not log any local diff fallback.
  $ cat "$TESTTMP/scs_scuba.json" | jq -r 'select(.normal.diff_fallback != null) | .normal.diff_fallback' | wc -l | sed 's/^[[:space:]]*//'
  0
