# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Holes in commit-graph SQL IDs, or IDs out of topological order, must not change
what Strider reports for each commit. See test-strider-chunked-deep-history.t
for the Walker and Strider arguments.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B-C-D
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # delete: B "dir1/file1"
  > # delete: B "dir1/sub/file3"
  > # delete: C "dir2/file2"
  > # modify: D "live" "live2"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)

Leave a gap of ten IDs before the next commits.

  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "UPDATE sqlite_sequence SET seq = seq + 10 WHERE name = 'commit_graph_edges'"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<EOF
  > D-E-F
  > # exists: D $D
  > # delete: E "live"
  > # modify: F "dir1/file1" "again"
  > # bookmark: F master
  > EOF
  D=* (glob)
  E=* (glob)
  F=* (glob)

  $ identities() {
  >   sed -nE \
  >     -e 's/^Node Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/Changeset:\1/p' \
  >     -e 's/^Node DeletedManifestV2Mapping\(ChangesetId\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2Mapping:\1/p' \
  >     -e 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2:\1/p' \
  >     -e 's/^(Changeset|DeletedManifestV2Mapping|DeletedManifestV2):([0-9a-f]+)$/\1:\2/p' \
  >   | sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g" \
  >     -e "s/$E/E/g" -e "s/$F/F/g" \
  >   | LC_ALL=C sort -u
  > }
  $ walker_scrub_derived() {
  >   mononoke_walker scrub -q \
  >     -I deep -i bonsai -i FileContent -i derived_deleted_manifest \
  >     -i derived_skeleton_manifests -i derived_changeset_info \
  >     -i FileContentMetadataV2 -i AliasContentMapping \
  >     --chunk-direction OldestFirst \
  >     --chunk-by-public Changeset --chunk-by-public ChangesetInfoMapping \
  >     --chunk-by-public DeletedManifestV2Mapping \
  >     --chunk-by-public SkeletonManifestMapping \
  >     --include-output-node-type Changeset \
  >     --include-output-node-type DeletedManifestV2Mapping \
  >     --include-output-node-type DeletedManifestV2 \
  >     --output-format Debug "$@" 2>&1 | identities
  > }
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
  >     --family core --family deleted-manifest \
  >     "$@" 2> strider.err
  > }
  $ strider_nodes() {
  >   grep -v ' status=' "$1" | cut -d' ' -f3 \
  >     | sed -e 's/^Mapping:deleted_manifest:/DeletedManifestV2Mapping:/' | identities
  > }
  $ completed() {
  >   grep 'Strider completed' strider.err | grep -cE 'status="?complete"?'
  > }

  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT id, lower(hex(cs_id)) FROM commit_graph_edges ORDER BY id" \
  >   | sed -e "s/$A/A/" -e "s/$B/B/" -e "s/$C/C/" -e "s/$D/D/" -e "s/$E/E/" -e "s/$F/F/"
  1|A
  2|B
  3|C
  4|D
  15|E
  16|F

One chunk covering every commit is the reference.

  $ walker_scrub_derived --chunk-size 100 > reference.nodes
  $ grep -E '^(Changeset|DeletedManifestV2Mapping):' reference.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  Changeset:D
  Changeset:E
  Changeset:F
  DeletedManifestV2Mapping:A
  DeletedManifestV2Mapping:B
  DeletedManifestV2Mapping:C
  DeletedManifestV2Mapping:D
  DeletedManifestV2Mapping:E
  DeletedManifestV2Mapping:F

Holes: the second chunk of three commits spans the gap. Every commit and
family is validated, and the union of each commit's additions is exactly the
reference.

  $ walker_scrub_derived --chunk-size 3 | diff -u reference.nodes -
  $ strider --chunk-size 3 > holes.out
  $ grep -c ' status=validated$' holes.out
  12
  $ strider_nodes holes.out | diff -u reference.nodes -
  $ completed
  1

Out of order: swap the SQL IDs of B and C, as a batched insert can, so child
C is enumerated before its parent B. Pointer columns are swapped too, so the
stored graph stays consistent.

  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT count(*) FROM commit_graph_merge_parents; SELECT count(*) FROM commit_graph_subtree_sources"
  0
  0
  $ swap() {
  >   echo "$1 = CASE $1 WHEN 2 THEN 3 WHEN 3 THEN 2 ELSE $1 END"
  > }
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "
  >   UPDATE commit_graph_edges SET id = -1 WHERE id = 2;
  >   UPDATE commit_graph_edges SET id = 2 WHERE id = 3;
  >   UPDATE commit_graph_edges SET id = 3 WHERE id = -1;
  >   UPDATE commit_graph_edges SET $(swap p1_parent), $(swap merge_ancestor),
  >     $(swap skip_tree_parent), $(swap skip_tree_skew_ancestor),
  >     $(swap p1_linear_skew_ancestor), $(swap subtree_or_merge_ancestor),
  >     $(swap subtree_source_parent), $(swap subtree_source_skew_ancestor);"
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT e.id, lower(hex(e.cs_id)), lower(hex(p.cs_id)) FROM commit_graph_edges e LEFT JOIN commit_graph_edges p ON p.id = e.p1_parent ORDER BY e.id" \
  >   | sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g" -e "s/$E/E/g" -e "s/$F/F/g"
  1|A|
  2|C|B
  3|B|A
  4|D|C
  15|E|D
  16|F|E

Chunked Walker misses a node here: it defers a deleted-manifest node linked to
B in C's chunk and never expands it. Strider reports exactly what it reported
before the swap, with a different chunk size.

  $ walker_scrub_derived --chunk-size 2 | LC_ALL=C comm -23 reference.nodes - | wc -l
  1
  $ strider --chunk-size 2 > swapped.out
  $ LC_ALL=C sort holes.out > holes.sorted
  $ LC_ALL=C sort swapped.out | diff -u holes.sorted -
  $ completed
  1
