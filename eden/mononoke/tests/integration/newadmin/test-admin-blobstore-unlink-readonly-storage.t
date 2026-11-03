# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.
#require slow

# `--with-readonly-storage=true` promises to error on any attempt to write to
# storage. `unlink` through a multiplexed WAL blobstore is a write, so it must
# be refused and the blob must survive.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ MULTIPLEXED=1 setup_common_config "blob_files"
  $ testtool_drawdag -R repo <<'EOF'
  > A
  > EOF
  A=* (glob)

Upload an arbitrary blob through the multiplex
  $ echo value > "$TESTTMP/value"
  $ mononoke_admin raw-blobstore --storage-name blobstore upload --key repo0000.somekey --value-file "$TESTTMP/value"
  Writing 6 bytes to blobstore key repo0000.somekey
  $ mononoke_admin raw-blobstore --storage-name blobstore fetch -q repo0000.somekey -o "$TESTTMP/fetched_value"
  $ diff "$TESTTMP/value" "$TESTTMP/fetched_value"

Unlinking with read-only storage is refused and the blob survives
  $ mononoke_admin --with-readonly-storage=true blobstore-unlink -R repo repo0000.somekey
  Failed to unlink key repo0000.somekey in one underlying blobstore, error: * (glob)
  $ mononoke_admin raw-blobstore --storage-name blobstore fetch -q repo0000.somekey -o "$TESTTMP/fetched_value_after"
  $ diff "$TESTTMP/value" "$TESTTMP/fetched_value_after"

Without read-only storage the unlink works
  $ mononoke_admin blobstore-unlink -R repo repo0000.somekey
  Unlinking key repo0000.somekey successfully in one underlying blobstore
  $ mononoke_admin raw-blobstore --storage-name blobstore fetch -q repo0000.somekey -o "$TESTTMP/fetched_value_unlinked"
  No blob exists for repo0000.somekey
