# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Contract: a successful `unlink` through the WAL multiplexed blobstore removes
# the key from EVERY component it writes to. Write-only components receive every
# `put` (after the write quorum is reached), and the WAL healer and the scrub
# blobstore copy whatever a component still holds back into the others, so a
# copy left in a write-only component resurrects an unlinked blob.
#
# FIXME: BUG! `WalMultiplexedBlobstore::unlink` only unlinks from the main
# components: after `blobstore-unlink` reports success the blob file is still
# present in the write-only component. This test pins the buggy behaviour; the
# next diff fixes it.

  $ . "${TEST_FIXTURES}/library.sh"

Setup a WAL multiplexed blobstore with two main components (0, 1) and add a
third, write-only component (2) to the generated storage config:
  $ MULTIPLEXED=1 setup_common_config "blob_files"
  $ mkdir -p "$TESTTMP/blobstore/2/blobs"
  $ sed -i 's|^  { blobstore_id = 1, \(.*\) },$|  { blobstore_id = 1, \1 },\n  { blobstore_id = 2, blobstore = { blob_files = { path = "'"$TESTTMP"'/blobstore/2" } }, store_type = { write_only = {} } },|' "$TESTTMP/mononoke-config/common/storage.toml"
  $ grep -A4 "^components" "$TESTTMP/mononoke-config/common/storage.toml"
  components = [
    { blobstore_id = 0, blobstore = { blob_files = { path = "$TESTTMP/blobstore/0" } } },
    { blobstore_id = 1, blobstore = { blob_files = { path = "$TESTTMP/blobstore/1" } } },
    { blobstore_id = 2, blobstore = { blob_files = { path = "$TESTTMP/blobstore/2" } }, store_type = { write_only = {} } },
  ]

Upload a blob through the multiplex: it lands in the main components and in the
write-only component.
  $ echo value > "$TESTTMP/value"
  $ mononoke_admin blobstore -R repo upload --key somekey --value-file "$TESTTMP/value"
  Writing 6 bytes to blobstore key somekey
  $ for i in 0 1 2; do echo "component $i: $(ls "$TESTTMP/blobstore/$i/blobs" | grep -c '^blob-repo0000.somekey$')"; done
  component 0: 1
  component 1: 1
  component 2: 1

Unlink the key through the multiplex. The unlink reports success:
  $ mononoke_admin blobstore-unlink -R repo repo0000.somekey
  Unlinking key repo0000.somekey successfully in one underlying blobstore
  $ mononoke_admin blobstore -R repo fetch -q somekey -o "$TESTTMP/fetched_value_unlinked"
  No blob exists for somekey

FIXME: BUG! The key is gone from the main components but still present in the
write-only component, from where the healer or scrub can copy it back:
  $ for i in 0 1 2; do echo "component $i: $(ls "$TESTTMP/blobstore/$i/blobs" | grep -c '^blob-repo0000.somekey$')"; done
  component 0: 0
  component 1: 0
  component 2: 1
