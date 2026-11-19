# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

Test-case: `ephemeral-store delete` removes the bubbles it is given, whether or not they have expired.
How/setup: mark-and-delete deletion mode, with snapshots whose bubbles (1 to 12) have not expired.
Expectation: every scenario below deletes only the bubbles it names.

  $ BUBBLE_DELETION_MODE=2 base_snapshot_repo_setup client1
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  $ cd client1
  $ mkdir test_tmp
  $ cd test_tmp
  $ echo "a file content" > a
  $ hg snapshot create
  snapshot: Snapshot created with id 1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a
  0 files modified, 0 files added, 0 files removed, 0 files missing, 1 files untracked
  $ echo "b file content" > b
  $ hg add b
  $ hg snapshot create
  snapshot: Snapshot created with id e1595c0f98b99ea1b9643675c3b2962bc7b7a2c48906c8e16539a86a2fb5dcf2
  0 files modified, 1 files added, 0 files removed, 0 files missing, 1 files untracked
  $ echo "c file content" > c
  $ hg snapshot create
  snapshot: Snapshot created with id 5451c3af64963d96b4b83546f8cb5989ff07a3e1edfec0c722bef810f62190ef
  0 files modified, 1 files added, 0 files removed, 0 files missing, 2 files untracked
  $ echo "d file content" > d
  $ echo "e file content" > e
  $ echo "f file content" > f
  $ echo "g file content" > g
  $ hg add g
  $ hg snapshot create
  snapshot: Snapshot created with id 613439997b9649b148bc9338357430252b6de0bd4b248cb28eb7fe7c08ab8d20
  0 files modified, 2 files added, 0 files removed, 0 files missing, 5 files untracked

Snapshots 5 to 9 each have their own bubble (5 to 9). Snapshot 10 starts bubble 10 and snapshot 11 continues it, so both share that bubble. Snapshot 12 has a label (bubble 11). Snapshot 13 is never named by any command (bubble 12).
The changeset IDs of snapshots 5 to 13 are saved as S5 to S13.
  $ for i in 5 6 7 8 9 10; do echo "file $i" > file$i; hg snapshot create | sed -n 's/.*created with id \([0-9a-f]*\).*/\1/p' >> "$TESTTMP/snapshot_ids"; done
  $ echo "file 11" > file11
  $ hg snapshot create --continuation-of latest | sed -n 's/.*created with id \([0-9a-f]*\).*/\1/p' >> "$TESTTMP/snapshot_ids"
  $ echo "file 12" > file12
  $ hg snapshot create --labels keep | sed -n 's/.*created with id \([0-9a-f]*\).*/\1/p' >> "$TESTTMP/snapshot_ids"
  $ echo "file 13" > file13
  $ hg snapshot create | sed -n 's/.*created with id \([0-9a-f]*\).*/\1/p' >> "$TESTTMP/snapshot_ids"
  $ S5=$(sed -n 1p "$TESTTMP/snapshot_ids"); S6=$(sed -n 2p "$TESTTMP/snapshot_ids"); S7=$(sed -n 3p "$TESTTMP/snapshot_ids"); S8=$(sed -n 4p "$TESTTMP/snapshot_ids"); S9=$(sed -n 5p "$TESTTMP/snapshot_ids")
  $ S10=$(sed -n 6p "$TESTTMP/snapshot_ids"); S11=$(sed -n 7p "$TESTTMP/snapshot_ids"); S12=$(sed -n 8p "$TESTTMP/snapshot_ids"); S13=$(sed -n 9p "$TESTTMP/snapshot_ids")
  $ cd ../../

The persistent blobs and the blobs of the bystander (bubble 12) are listed to check that they are left untouched at the end.
  $ ls blobstore/blobs | grep -v -- '-eph' | sort > "$TESTTMP/persistent_blobs_before"
  $ ls blobstore/blobs | grep -- '-eph12\.' | sort > "$TESTTMP/bystander_blobs_before"
  $ [ -s "$TESTTMP/bystander_blobs_before" ] && echo "Bystander has blobs"
  Bystander has blobs

Test-case: a dry run lists the bubble of a changeset and deletes nothing.
How/setup: dry-run delete of the changeset of bubble 1.
Expectation: the bubble stays active, keeps its changeset mapping, and its blob stays.

  $ mononoke_admin ephemeral-store -R repo delete -n -i 1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a
  Bubble 1 contains changesets: [1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a]
  Executing delete in dry-run mode. None of the bubbles above were deleted.

  $ mononoke_admin ephemeral-store -R repo info -i 1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a | grep -E "BubbleID|Status"
  BubbleID: 1
  Status: Active
  $ [ -f "blobstore/blobs/blob-eph1.repo0000.changeset.blake2.1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a" ] && echo "Blob still present"
  Blob still present

Test-case: a changeset's bubble is deleted even though it has not expired.
How/setup: delete the changeset of bubble 1.
Expectation: the blobs, the bubble and its changeset mapping are gone, and the other bubbles are untouched.

  $ mononoke_admin ephemeral-store -R repo delete -i 1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a
  Bubble 1 contains changesets: [1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a]
  Cleaned up bubble 1 and deleted * blob keys contained in it (glob)
  Deleted 1 of 1 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)

  $ [ -f "blobstore/blobs/blob-eph1.repo0000.changeset.blake2.1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a" ] && echo "Blob still present"
  [1]
  $ mononoke_admin ephemeral-store -R repo info -b 1
  Error: bubble 1 does not exist, or has expired
  [1]
  $ mononoke_admin ephemeral-store -R repo info -i 1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a
  Error: No bubble exists for changeset ID 1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a
  [1]
  $ mononoke_admin ephemeral-store -R repo info -b 2 | grep Status
  Status: Active

Test-case: bubble IDs and changeset IDs can each be repeated on the command line.
How/setup: delete bubbles 2 and 3 with `-b`, and the bubbles of snapshots 5 and 6 with `-i`.
Expectation: exactly those four bubbles are deleted.

  $ mononoke_admin ephemeral-store -R repo delete -b 2 -b 3 -i "$S5" -i "$S6"
  Bubble 2 contains changesets: [e1595c0f98b99ea1b9643675c3b2962bc7b7a2c48906c8e16539a86a2fb5dcf2]
  Bubble 3 contains changesets: [5451c3af64963d96b4b83546f8cb5989ff07a3e1edfec0c722bef810f62190ef]
  Bubble 5 contains changesets: [*] (glob)
  Bubble 6 contains changesets: [*] (glob)
  Cleaned up bubble * and deleted * blob keys contained in it (glob)
  Cleaned up bubble * and deleted * blob keys contained in it (glob)
  Cleaned up bubble * and deleted * blob keys contained in it (glob)
  Cleaned up bubble * and deleted * blob keys contained in it (glob)
  Deleted 4 of 4 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)

Test-case: an IDs file that is rejected deletes nothing.
How/setup: pass bubble 4 on the command line together with a missing file, malformed JSON, an unknown field, and an invalid changeset ID.
Expectation: each fails with its own error, and bubble 4 stays active.

  $ mononoke_admin ephemeral-store -R repo delete -b 4 --ids-file missing.json
  Error: Failed to read missing.json
  
  Caused by:
      No such file or directory (os error 2)
  [1]
  $ echo "not json" > malformed.json
  $ mononoke_admin ephemeral-store -R repo delete -b 4 --ids-file malformed.json
  Error: Failed to parse malformed.json as an IDs file
  
  Caused by:
      expected ident at line 1 column 2
  [1]
  $ echo '{"bubbles": [4]}' > unknown_field.json
  $ mononoke_admin ephemeral-store -R repo delete -b 4 --ids-file unknown_field.json
  Error: Failed to parse unknown_field.json as an IDs file
  
  Caused by:
      unknown field `bubbles`, expected `bubble_ids` or `changeset_ids` at line 1 column 10
  [1]
  $ echo '{"changeset_ids": ["not_a_changeset"]}' > invalid_id.json
  $ mononoke_admin ephemeral-store -R repo delete -b 4 --ids-file invalid_id.json
  Error: Failed to parse invalid_id.json as an IDs file
  
  Caused by:
      * (glob)
  [1]
  $ mononoke_admin ephemeral-store -R repo info -b 4 | grep Status
  Status: Active

Test-case: an IDs file may hold only bubble IDs, or only changeset IDs.
How/setup: delete bubble 4 with a file that has only `bubble_ids`, and the bubble of snapshot 7 with a file that has only `changeset_ids`.
Expectation: each file deletes its bubble.

  $ echo '{"bubble_ids": [4]}' > only_bubbles.json
  $ mononoke_admin ephemeral-store -R repo delete --ids-file only_bubbles.json
  Bubble 4 contains changesets: [613439997b9649b148bc9338357430252b6de0bd4b248cb28eb7fe7c08ab8d20]
  Cleaned up bubble 4 and deleted * blob keys contained in it (glob)
  Deleted 1 of 1 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)
  $ echo "{\"changeset_ids\": [\"$S7\"]}" > only_changesets.json
  $ mononoke_admin ephemeral-store -R repo delete --ids-file only_changesets.json
  Bubble 7 contains changesets: [*] (glob)
  Cleaned up bubble 7 and deleted * blob keys contained in it (glob)
  Deleted 1 of 1 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)

Test-case: IDs given on the command line and in an IDs file are combined, and a bubble named twice is deleted once.
How/setup: delete bubble 8 with `-b`, and again in the file together with the changeset of snapshot 9.
Expectation: bubbles 8 and 9 are deleted, once each.

  $ echo "{\"bubble_ids\": [8], \"changeset_ids\": [\"$S9\"]}" > combined.json
  $ mononoke_admin ephemeral-store -R repo delete -b 8 --ids-file combined.json
  Bubble 8 contains changesets: [*] (glob)
  Bubble 9 contains changesets: [*] (glob)
  Cleaned up bubble * and deleted * blob keys contained in it (glob)
  Cleaned up bubble * and deleted * blob keys contained in it (glob)
  Deleted 2 of 2 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)

Test-case: deleting one snapshot of a chain deletes the whole bubble, and the dry run lists every snapshot that would go.
How/setup: snapshots 10 and 11 share bubble 10; dry-run, then delete, the changeset of snapshot 11.
Expectation: the dry run lists both changesets, says which one was not requested, and deletes nothing; the delete removes both snapshots.

  $ mononoke_admin ephemeral-store -R repo delete -n -i "$S11"
  Bubble 10 contains changesets: [*, *] (glob)
  Bubble 10 will also delete changesets that were not requested: [*] (glob)
  Executing delete in dry-run mode. None of the bubbles above were deleted.
The changeset that was not requested is snapshot 10, not snapshot 11:
  $ mononoke_admin ephemeral-store -R repo delete -n -i "$S11" | grep "not requested" | grep -c "$S10"
  1
  $ mononoke_admin ephemeral-store -R repo delete -n -i "$S11" | grep "not requested" | grep -c "$S11"
  0
  [1]
Naming the bubble itself asks for all of its changesets, so none is reported as not requested:
  $ mononoke_admin ephemeral-store -R repo delete -n -b 10 | grep -c "not requested"
  0
  [1]
  $ mononoke_admin ephemeral-store -R repo info -i "$S10" | grep Status
  Status: Active

  $ mononoke_admin ephemeral-store -R repo delete -i "$S11"
  Bubble 10 contains changesets: [*, *] (glob)
  Bubble 10 will also delete changesets that were not requested: [*] (glob)
  Cleaned up bubble 10 and deleted * blob keys contained in it (glob)
  Deleted 1 of 1 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)
  $ mononoke_admin ephemeral-store -R repo info -i "$S10"
  Error: No bubble exists for changeset ID * (glob)
  [1]

Test-case: a labelled snapshot, which never expires, is deleted when it is named.
How/setup: snapshot 12 has the label `keep`; delete its changeset.
Expectation: the bubble is deleted despite the label.

  $ mononoke_admin ephemeral-store -R repo delete -i "$S12"
  Bubble 11 contains changesets: [*] (glob)
  Cleaned up bubble 11 and deleted * blob keys contained in it (glob)
  Deleted 1 of 1 bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)
  $ mononoke_admin ephemeral-store -R repo info -b 11
  Error: bubble 11 does not exist, or has expired
  [1]

Test-case: a request that names something that does not exist deletes nothing.
How/setup: name the bystander (bubble 12) together with a changeset that has no bubble, a bubble that does not exist, in dry-run and real runs, and name only a missing bubble or changeset; also pass no IDs, zero concurrency.
Expectation: each fails loudly, none deletes anything (the bystander is checked afterwards), and the invalid invocations are rejected.

  $ mononoke_admin ephemeral-store -R repo delete -b 12 -i 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5
  Error: No bubble exists for changeset IDs: 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5. Nothing was deleted
  [1]
  $ mononoke_admin ephemeral-store -R repo delete -n -i 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5
  Error: No bubble exists for changeset IDs: 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5. Nothing was deleted
  [1]
  $ mononoke_admin ephemeral-store -R repo delete -b 12 -b 100001 -j 1
  Error: Cannot delete bubbles that cannot be read. Nothing was deleted:
    Bubble 100001: bubble 100001 does not exist, or has expired
  [1]
  $ mononoke_admin ephemeral-store -R repo delete -n -b 100001
  Error: Cannot delete bubbles that cannot be read. Nothing was deleted:
    Bubble 100001: bubble 100001 does not exist, or has expired
  [1]
  $ echo "{\"bubble_ids\": [12], \"changeset_ids\": [\"49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5\"]}" > mixed.json
  $ mononoke_admin ephemeral-store -R repo delete --ids-file mixed.json
  Error: No bubble exists for changeset IDs: 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5. Nothing was deleted
  [1]
  $ mononoke_admin ephemeral-store -R repo delete
  Error: Need to provide bubble IDs, changeset IDs or an IDs file
  [1]
  $ mononoke_admin ephemeral-store -R repo delete -b 12 -j 0 2>&1 | grep "invalid value"
  error: invalid value '0' for '--concurrency <CONCURRENCY>': number would be zero for non-zero type

Test-case: bubbles that are not named are left untouched by all the deletions and failed requests above.
How/setup: snapshot 13 (bubble 12) was never named by a successful request; compare the blobs against the lists taken before any deletion.
Expectation: the bystander is still active with all its blobs, and no persistent blob changed.

  $ mononoke_admin ephemeral-store -R repo info -i "$S13" | grep Status
  Status: Active
  $ ls blobstore/blobs | grep -- '-eph12\.' | sort | diff - "$TESTTMP/bystander_blobs_before" && echo "Bystander blobs unchanged"
  Bystander blobs unchanged
  $ ls blobstore/blobs | grep -v -- '-eph' | sort | diff - "$TESTTMP/persistent_blobs_before" && echo "Persistent blobs unchanged"
  Persistent blobs unchanged
