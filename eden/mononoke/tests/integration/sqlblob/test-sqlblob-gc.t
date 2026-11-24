# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ BLOB_TYPE="blob_sqlite" default_setup_drawdag
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Check that sqlblob has some data big enough to form a chunk
  $ for s in 0 1; do sqlite3 -readonly "$TESTTMP/blobstore/blobs/shard_${s}.sqlite" "SELECT COUNT(1) FROM chunk" ; done
  3
  6

Check that chunk_generations populated from put and they have length
  $ for s in 0 1; do sqlite3 -readonly "$TESTTMP/blobstore/blobs/shard_${s}.sqlite" "SELECT * FROM chunk_generation ORDER BY id" | sed "s/^/$s /"; done
  0 13642c09964c02b0289b1f8016ea655e0c76652c3704bdab7ed95e1f30a030cb|2|303
  0 65f5291738cd2b344a37df455097ac5e2fac677c4ad8a4d7fa86bb53ea3e359f|2|209
  0 a70d30e8fc07284fe65d048ca12ab231a5af2a4585fc95d1c681764c928bd302|2|375
  1 0a9821efd59e32cc4aee9ea652eb4918ba2bd4f744df912561c89b329ddb8f35|2|398
  1 771d08e57a60be5757e98b9c8cabaaed5e02034e38507df4a650254a8545c4d4|2|199
  1 9061e5b04e9a083f3bab0582265b214951060e1d2372377cf16f0d1b43d61343|2|208
  1 e7e77bea18f290767854dcacbaad963afd80656dbcb1c00dd7e36375e0408eaa|2|274
  1 f1183f46ffc43ce1662e551dd09ab870e7051d25c9b6cfdaa0d0d2a5d8b5e8fb|2|224
  1 f50e7c0f6932bdffdf0265beb40720a074b6168cc3ae28c372637d78114f86ae|2|194

Run sqlblob_gc generation size report
  $ mononoke_sqlblob_gc --storage-config-name=blobstore --shard-count=2 generation-size
  Generation | Size
  -----------------
           2 | 2.3 KiB
  Total size: 2.3 KiB

A marker that expects another mark generation refuses, and exits non-zero, before touching any shard
  $ (set -o pipefail; mononoke_sqlblob_gc --storage-config-name=blobstore --shard-count=2 mark --expected-mark-generation 2)
  Error: Refusing to mark: the configured mark generation is 1, expected 2
  [1]

Run sqlblob_gc mark
  $ mononoke_sqlblob_gc --storage-config-name=blobstore --shard-count=2 mark
  [INFO] Starting initial generation set
  [INFO] Completed initial generation handling on shard * (glob)
  [INFO] Completed initial generation handling on shard * (glob)
  [INFO] Completed initial generation set
  [INFO] Starting marking generation 1
  [INFO] Starting mark on data keys from shard * (glob)
  [INFO] Starting mark on data keys from shard * (glob)
  [INFO] Completed marking generation 1

Run sqlblob_gc generation size report again, just to check mark has not broken it
  $ mononoke_sqlblob_gc --storage-config-name=blobstore --shard-count=2 --scuba-log-file scuba.json generation-size
  Generation | Size
  -----------------
           2 | 2.3 KiB
  Total size: 2.3 KiB

Check the sizes are logged
  $ jq -r '.int | [ .shard, .generation, .size, .chunk_id_count, .storage_total_footprint ] | @csv' < scuba.json | sort
  ,,,,2384
  0,2,887,3,
  1,2,1497,6,
