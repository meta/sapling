# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Over full history, Strider must read the same blobstore keys as the deployed
scrub-all Walker job, attributed to the same types, for every type Strider
audits. Walker's attribution comes from its pack rows, which carry each read
key's node type; Strider's from `--print-reads`, which prints each read that
reaches a store with the type of the load that made it. Strider names a
mapping by its derived data type, so `Mapping:unodes` is Walker's
`UnodeMapping` and `Mapping:deleted_manifest` its `DeletedManifestV2Mapping`.

J adds 2,100 files to one directory and K deletes them, so K's deleted
manifest stores that directory in sharded-map nodes. Strider types those
nodes `DeletedManifestV2MapNode`; Walker counts them under the
`DeletedManifestV2` node whose expansion reads them.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ {
  >   cat <<'EOF'
  >     K
  >     |
  >     J
  >     |
  >     H
  >    / \
  >   F   G
  >   |   |
  >   D   E
  >    \ /
  >     C
  >     |
  >     B
  >     |
  >     A
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # modify: B "dir1/file1" "one-b"
  > # modify: C "dir1/sub/file3" "three-c"
  > # delete: D "dir1/sub/file3"
  > # modify: E "live" "live-e"
  > # modify: F "dir2/file2" "two-f"
  > # delete: G "dir1/file1"
  > # modify: H "live" "live-h"
  > # modify: H "dir1/file1" "one-h"
  > # modify: H "dir2/file2" "two-f"
  > # delete: H "dir1/sub/file3"
  > # bookmark: K master
  > EOF
  >   for i in $(seq 1000 3099); do
  >     echo "# modify: J \"many/f$i\" \"$i\""
  >     echo "# delete: K \"many/f$i\""
  >   done
  > } > dag
  $ testtool_drawdag -R repo --derive-all --no-default-files < dag 2> drawdag.err
  $ grep -c '^[A-K]=' drawdag.err
  10

Walker runs with the scrub-all job's graph and chunking arguments, plus pack
logging; checkpoint, tailing, replica and QPS arguments are omitted. Only the
types Strider audits are compared.

  $ audited_types='^(Changeset|FileContent|UnodeMapping|UnodeManifest|UnodeFile|DeletedManifestV2Mapping|DeletedManifestV2) '
  $ mononoke_walker scrub -q \
  >   -I deep -i bonsai -i all -I marker -x Blame \
  >   --chunk-direction OldestFirst --chunk-by-public Changeset --chunk-size 100 \
  >   --include-pack-log-node-type all --pack-log-scuba-file walker-packs.json > walker.out 2>&1
  $ jq -r '.normal | "\(.node_type) \(.blobstore_key)"' < walker-packs.json \
  >   | grep -E "$audited_types" | LC_ALL=C sort -u > walker-all.reads

Walker's `UnodeMapping` step also reads each commit's fastlog mapping, because
fastlog is enabled. Fastlog is its own family in Strider and is not audited
yet, so those keys are left out.

  $ grep -c '^UnodeMapping repo0000\.derived_rootfastlog\.' walker-all.reads
  10
  $ grep -v '^UnodeMapping repo0000\.derived_rootfastlog\.' walker-all.reads > walker.reads

  $ strider() {
  >   render_mononoke_config
  >   GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4 \
  >     "$@"
  > }
  $ strider --chunk-size 100 --family core --family unode --family deleted-manifest \
  >   --print-reads > strider.out 2> strider.err

Every read Strider made was tagged, and this repository's setup reads nothing
from storage.

  $ grep 'Strider completed' strider.err | grep -E 'status="?complete"?' | grep -E 'untagged_reads=0 ' | grep -cE 'reads_outside_audit=0( |$)'
  1

Each type's keys match Walker's.

  $ grep '^read ' strider.out | grep -v ' absent$' | cut -d' ' -f3,4 \
  >   | sed -e 's/^Mapping:unodes /UnodeMapping /' \
  >     -e 's/^Mapping:deleted_manifest /DeletedManifestV2Mapping /' \
  >     -e 's/^DeletedManifestV2MapNode /DeletedManifestV2 /' \
  >   | LC_ALL=C sort -u > strider.reads
  $ cut -d' ' -f1 walker.reads | uniq -c
       10 Changeset
       17 DeletedManifestV2
       10 DeletedManifestV2Mapping
     2110 FileContent
     2110 UnodeFile
       21 UnodeManifest
       10 UnodeMapping
  $ diff -u walker.reads strider.reads

Strider logs a total for every type it read, sharded-map nodes included.

  $ grep 'Strider reads' strider.err | sed -nE 's/.*object_type="?([^" ]*)"?.*/\1/p'
  Changeset
  DeletedManifestV2
  DeletedManifestV2MapNode
  FileContent
  Mapping:deleted_manifest
  Mapping:unodes
  UnodeFile
  UnodeManifest
