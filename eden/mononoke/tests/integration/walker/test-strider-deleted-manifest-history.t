# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Delete files in different commits so C reuses the deleted-manifest subtree B
added.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B-C
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # delete: B "dir1/file1"
  > # delete: C "dir2/file2"
  > # bookmark: C master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

Run a deep Walker traversal from C's deleted-manifest mapping, including
changesets so that parent and mapping edges are followed.

  $ mononoke_walker scrub -q \
  >   --walk-root "DeletedManifestV2Mapping:$C" \
  >   -I deep \
  >   -i bonsai \
  >   -i derived_deleted_manifest \
  >   --include-output-node-type Changeset \
  >   --include-output-node-type DeletedManifestV2Mapping \
  >   --include-output-node-type DeletedManifestV2 \
  >   --output-format Debug > walker.out 2>&1
  $ grep '^\[INFO\] Walking edge types' walker.out
  [INFO] Walking edge types [ChangesetToBonsaiParent, ChangesetToDeletedManifestV2Mapping, DeletedManifestV2MappingToRootDeletedManifestV2, DeletedManifestV2ToDeletedManifestV2Child, DeletedManifestV2ToLinkedChangeset]
  $ sed -nE \
  >   -e 's/^Node Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/Changeset:\1/p' \
  >   -e 's/^Node DeletedManifestV2Mapping\(ChangesetId\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2Mapping:\1/p' \
  >   -e 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2:\1/p' \
  >   walker.out | sed -e "s/$A/A/" -e "s/$B/B/" -e "s/$C/C/" | LC_ALL=C sort -u > walker.nodes
  $ grep -E '^(Changeset|DeletedManifestV2Mapping):' walker.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  DeletedManifestV2Mapping:A
  DeletedManifestV2Mapping:B
  DeletedManifestV2Mapping:C

Run Strider over every commit in SQL-ID order with the core and
deleted-manifest families. File contents have no counterpart in this Walker
run, so they are left out of the comparison.

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
  >     --print-visited-nodes \
  >     --family core \
  >     --family deleted-manifest \
  >     "$@"
  > }
  $ letters() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" \
  >     -e 's/Mapping:deleted_manifest:/DeletedManifestV2Mapping:/'
  > }
  $ strider --chunk-size 2 > strider.out 2> strider.err
  $ grep 'Strider completed' strider.err | grep -E 'status="?complete"?' | grep -E ' commits=3 ' > /dev/null

Each commit and family is validated, and each commit adds its changeset, its
mapping, and only the manifest nodes that differ from its parent. C reuses B's
`dir1` subtree.

  $ grep status= strider.out | letters | LC_ALL=C sort
  A core status=validated
  A deleted-manifest status=validated
  B core status=validated
  B deleted-manifest status=validated
  C core status=validated
  C deleted-manifest status=validated
  $ grep -v status= strider.out | letters | cut -d' ' -f1,2 | LC_ALL=C sort | uniq -c
        4 A core
        2 A deleted-manifest
        1 B core
        4 B deleted-manifest
        1 C core
        4 C deleted-manifest

Over full history, the union of each commit's additions is exactly Walker's
deep walk.

  $ grep -v status= strider.out | letters | cut -d' ' -f3 | grep -v '^FileContent:' | LC_ALL=C sort -u > strider.nodes
  $ diff -u walker.nodes strider.nodes

Resume after A. Strider audits only B and C and reports the same additions for
them as the full run, without reporting anything from below the range.

  $ LOWER=$(( $(sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT id FROM commit_graph_edges WHERE cs_id = x'$A'") + 1 ))
  $ strider --chunk-size 2 --repo-lower-bound "$LOWER" > resume.out 2> resume.err
  $ grep -E "^($B|$C) " strider.out | LC_ALL=C sort > expected-resume
  $ LC_ALL=C sort resume.out | diff -u expected-resume -
