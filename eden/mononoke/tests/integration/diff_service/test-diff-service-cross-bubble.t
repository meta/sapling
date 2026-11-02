# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for diff_service cross-bubble snapshot comparisons.
# This tests the fix that allows comparing snapshots from different bubbles.
#
# Previously, cross-bubble comparisons would fail because the diff
# implementation used a single blobstore for both inputs. Each bubble has
# its own isolated storage, so comparing across bubbles required using
# separate repo contexts for each input.
#
# After the fix, the diff_service creates per-input repo contexts,
# allowing cross-bubble comparisons to work correctly.

setup
  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

  $ INFINITEPUSH_ALLOW_WRITES=true REPONAME="repo" setup_common_config

Enable hg snapshot extension
  $ cd "$TESTTMP"
  $ cat >> "$HGRCPATH" <<EOF
  > [extensions]
  > snapshot =
  > commitcloud =
  > amend =
  > EOF

Create a test repository with files
  $ testtool_drawdag -R repo --print-hg-hashes <<'EOF'
  > A-B
  > # bookmark: B main
  > # modify: A "file.txt" "original content\n"
  > # modify: B "file.txt" "modified content\n"
  > EOF
  A=* (glob)
  B=* (glob)

Convert HG hashes to bonsai for diff_service commands
  $ B_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $B 2>/dev/null)

Start mononoke for snapshot operations
  $ mononoke
  $ wait_for_mononoke

Clone repo and create first snapshot
  $ hg clone -q mono:repo client1
  $ cd client1
  $ hg checkout $B -q

Create local changes for the first snapshot (bubble 1)
  $ echo "snapshot 1 content" > file.txt

Create the first snapshot
  $ SNAPSHOT1_ID=$(HGPLAIN=1 hg snapshot create)
  $ echo "Snapshot 1 ID:"
  Snapshot 1 ID:
  $ echo $SNAPSHOT1_ID
  * (glob)
  $ cd ..

Create a second snapshot with different changes (bubble 2)
Each hg snapshot create creates a new bubble, so this will be in a different bubble
  $ cd client1
  $ hg checkout $B -q
  $ echo "snapshot 2 content" > file.txt

Create the second snapshot (in a new bubble)
  $ SNAPSHOT2_ID=$(HGPLAIN=1 hg snapshot create)
  $ echo "Snapshot 2 ID:"
  Snapshot 2 ID:
  $ echo $SNAPSHOT2_ID
  * (glob)
  $ cd ..

Start diff service
  $ start_and_wait_for_diff_service
  $ diff_service_client status
  Service status: fb303_status::ALIVE

== Test cross-bubble diff with diff_service ==

This is the key test - comparing two snapshots from different bubbles.
Previously this would fail with a content not found error because the
implementation used a single blobstore for both inputs.

Test: Diff hunks between two snapshots from different bubbles
  $ diff_service_client diff-hunks --repo-name repo --base-changeset-id "$SNAPSHOT1_ID" --base-path file.txt --other-changeset-id "$SNAPSHOT2_ID" --other-path file.txt
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-1)
  === End of hunks ===

Test: Unified diff between two snapshots from different bubbles
  $ diff_service_client diff-unified --repo-name repo --base-changeset-id "$SNAPSHOT1_ID" --base-path file.txt --other-changeset-id "$SNAPSHOT2_ID" --other-path file.txt
  * (glob)
  diff --git a/file.txt b/file.txt
  --- a/file.txt
  +++ b/file.txt
  @@ -1,1 +1,1 @@
  -snapshot 1 content
  +snapshot 2 content
  === End of unified diff ===

Test: Headerless unified diff between two snapshots from different bubbles
  $ diff_service_client diff-unified-headerless --repo-name repo --base-changeset-id "$SNAPSHOT1_ID" --base-path file.txt --other-changeset-id "$SNAPSHOT2_ID" --other-path file.txt
  Headerless diff output (is_binary: false, num_chunks: 1):
  @@ -1,1 +1,1 @@
  -snapshot 2 content
  +snapshot 1 content
  === End of headerless diff ===

Test: Metadata diff between two snapshots from different bubbles
  $ diff_service_client metadata-diff --repo-name repo --base-changeset-id "$SNAPSHOT1_ID" --base-path file.txt --other-changeset-id "$SNAPSHOT2_ID" --other-path file.txt
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

== Test cross-bubble with regular commit ==

Test: Diff between regular commit and snapshot from a bubble
  $ diff_service_client diff-hunks --repo-name repo --base-changeset-id "$B_BONSAI" --base-path file.txt --other-changeset-id "$SNAPSHOT1_ID" --other-path file.txt
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-1)
  === End of hunks ===

Test: Diff between snapshot and regular commit
  $ diff_service_client diff-hunks --repo-name repo --base-changeset-id "$SNAPSHOT2_ID" --base-path file.txt --other-changeset-id "$B_BONSAI" --other-path file.txt
  Hunks output (num_hunks: 1):
  Hunk: add_range(0-1), remove_range(0-1)
  === End of hunks ===

== Test commit-compare cross-bubble ==

Test: commit-compare two snapshots from different bubbles (paths-only)
  $ diff_service_client commit-compare --repo-name repo --commit-id "$SNAPSHOT1_ID" --other-commit-id "$SNAPSHOT2_ID" --paths-only
  M file.txt

Test: commit-compare regular commit vs snapshot (paths-only)
  $ diff_service_client commit-compare --repo-name repo --commit-id "$B_BONSAI" --other-commit-id "$SNAPSHOT1_ID" --paths-only
  M file.txt

Test: commit-compare snapshot vs regular commit (paths-only)
  $ diff_service_client commit-compare --repo-name repo --commit-id "$SNAPSHOT1_ID" --other-commit-id "$B_BONSAI" --paths-only
  M file.txt
