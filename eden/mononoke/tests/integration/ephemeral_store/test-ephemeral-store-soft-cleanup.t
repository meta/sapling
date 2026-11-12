# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

setup configuration
# Need short-term expiring bubbles with mark-and-delete deletion mode
  $ BUBBLE_DELETION_MODE=1 BUBBLE_EXPIRATION_SECS=0 BUBBLE_LIFESPAN_SECS=10 base_snapshot_repo_setup client1
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
  $ hg snapshot create
  snapshot: Snapshot created with id 9693167f9813f3b93de764cc5ba906b140e6e7464f6dac649e83135d88ae59e2
  0 files modified, 1 files added, 0 files removed, 0 files missing, 3 files untracked
# Ensure bubbles are expired before moving forward
  $ sleep 10

Cleanup expired bubbles in dry-run mode:
  $ mononoke_admin ephemeral-store -R repo cleanup -n -c 0
  Fetched 4 expired bubbles for deletion
  Executing cleanup in dry-run mode. The following bubbles were fetched for deletion:
  [BubbleId(1), BubbleId(2), BubbleId(3), BubbleId(4)]

Cleanup expired bubbles with too high cut-off
  $ mononoke_admin ephemeral-store -R repo cleanup -n -c 100000
  No expired bubbles found for deletion based on input provided

Cleanup expired bubbles with zero as the limit
  $ mononoke_admin ephemeral-store -R repo cleanup -l 0
  No expired bubbles found for deletion based on input provided

Cleanup expired bubbles in dry-run mode with non-zero as the limit
  $ mononoke_admin ephemeral-store -R repo cleanup -l 2 -n -c 0
  Fetched 2 expired bubbles for deletion
  Executing cleanup in dry-run mode. The following bubbles were fetched for deletion:
  [BubbleId(1), BubbleId(2)]

Before doing actual clean-up, verify that the blob indeed exists
  $ cd ../../
  $ [ -f "blobstore/blobs/blob-eph1.repo0000.changeset.blake2.1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a" ] && echo "Blob still present"
  Blob still present

Cleanup expired bubbles with no cut-off
  $ mononoke_admin ephemeral-store -R repo cleanup -c 0
  Fetched 4 expired bubbles for deletion
  Cleaned up bubble 1 and deleted 0 blob keys contained in it
  Cleaned up bubble 2 and deleted 0 blob keys contained in it
  Cleaned up bubble 3 and deleted 0 blob keys contained in it
  Cleaned up bubble 4 and deleted 0 blob keys contained in it
  Deleted 4 of 4 expired bubbles in *, 0 of them had blob keys that were already deleted, 0 failed (glob)

# Since deletion_mode=MARK_ONLY, the blob keys within the bubbles
# should NOT be deleted
Verify the blob keys are NOT actually deleted (using a key from Bubble 1).
  $ [ -f "blobstore/blobs/blob-eph1.repo0000.changeset.blake2.1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a" ] && echo "Blob still present"
  Blob still present
