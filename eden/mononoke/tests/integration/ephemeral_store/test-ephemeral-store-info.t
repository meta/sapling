# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

setup configuration
  $ base_snapshot_repo_setup client1
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

Fetch info about a bubble that exists:
  $ mononoke_admin ephemeral-store -R repo info -b 1
  BubbleID: 1
  ChangesetIDs: [ChangesetId(Blake2(1339d6c4160270f23c93227c8d3616fe78baec889f913de1408160869c18368a))]
  RepoID: 0
  ExpiryDate: *+00:00 (glob)
  Status: Active
  BlobstorePrefix: eph1.

Fetch info about a bubble that doesn't exist:
  $ mononoke_admin ephemeral-store -R repo info -b 100001
  Error: bubble 100001 does not exist, or has expired
  [1]

Fetch info about a bubble based on a valid changeset ID:
  $ mononoke_admin ephemeral-store -R repo info -i e1595c0f98b99ea1b9643675c3b2962bc7b7a2c48906c8e16539a86a2fb5dcf2
  BubbleID: 2
  ChangesetIDs: [ChangesetId(Blake2(e1595c0f98b99ea1b9643675c3b2962bc7b7a2c48906c8e16539a86a2fb5dcf2))]
  RepoID: 0
  ExpiryDate: *+00:00 (glob)
  Status: Active
  BlobstorePrefix: eph2.

Fetch info about a bubble based on an invalid changeset ID:
  $ mononoke_admin ephemeral-store -R repo info -i ofcourse_this_is_invalid
  Error: invalid blake2 input: need exactly 64 hex digits
  [1]
Fetch info about a bubble based on a non-matching changeset ID:
  $ mononoke_admin ephemeral-store -R repo info -i 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5
  Error: No bubble exists for changeset ID 49c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5
  [1]
