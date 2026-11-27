# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Over full history, the union of what each commit added must be exactly the
Changeset, DeletedManifestV2Mapping and DeletedManifestV2 nodes that Walker
visits in the deployed scrub-derived and scrub-all jobs. Walker runs with those
jobs' graph and chunking arguments; checkpoint, tailing, pack logging, replica
and QPS arguments are omitted.

Each Walker job maps to the Strider families it covers: scrub-derived to
`core` and `deleted-manifest`, and scrub-all to those and `unode`. Strider's
commit source is an SQL-ID range given by `--chunk-size` with an optional
`--repo-lower-bound`.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
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
  > # modify: A "x/file" "x1"
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # delete: B "x/file"
  > # delete: B "dir1/file1"
  > # delete: B "dir1/sub/file3"
  > # modify: D "x/file" "x2"
  > # delete: E "dir2/file2"
  > # delete: F "x/file"
  > # modify: G "live" "live2"
  > # modify: H "live" "live2"
  > # delete: H "dir2/file2"
  > # modify: H "merged" "merged"
  > # bookmark: H master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)
  E=* (glob)
  F=* (glob)
  G=* (glob)
  H=* (glob)

  $ identities() {
  >   sed -nE \
  >     -e 's/^Node Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/Changeset:\1/p' \
  >     -e 's/^Node DeletedManifestV2Mapping\(ChangesetId\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2Mapping:\1/p' \
  >     -e 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2:\1/p' \
  >     -e 's/^(Changeset|DeletedManifestV2Mapping|DeletedManifestV2):([0-9a-f]+)$/\1:\2/p' \
  >   | sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g" \
  >     -e "s/$E/E/g" -e "s/$F/F/g" -e "s/$G/G/g" -e "s/$H/H/g" \
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
  $ walker_scrub_all() {
  >   mononoke_walker scrub -q \
  >     -I deep -i bonsai -i all -I marker -x Blame \
  >     --chunk-direction OldestFirst --chunk-by-public Changeset \
  >     --include-output-node-type Changeset \
  >     --include-output-node-type DeletedManifestV2Mapping \
  >     --include-output-node-type DeletedManifestV2 \
  >     --output-format Debug "$@" 2>&1 | identities
  > }
  $ strider_scrub_derived=(--operation walk --family core --family deleted-manifest)
  $ strider_scrub_all=("${strider_scrub_derived[@]}" --family unode)
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
  >     "$@" 2> strider.err
  > }
  $ strider_nodes() {
  >   grep -v ' status=' "$1" | cut -d' ' -f3 \
  >     | sed -e 's/^Mapping:deleted_manifest:/DeletedManifestV2Mapping:/' | identities
  > }
  $ completed() {
  >   grep 'Strider completed' strider.err | grep -cE 'status="?complete"?'
  > }

Walker's one-chunk walk is the reference. It reaches every commit, every
mapping, and more than one deleted-manifest node per deletion.

  $ walker_scrub_derived --chunk-size 100 > walker.nodes
  $ grep -E '^(Changeset|DeletedManifestV2Mapping):' walker.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  Changeset:D
  Changeset:E
  Changeset:F
  Changeset:G
  Changeset:H
  DeletedManifestV2Mapping:A
  DeletedManifestV2Mapping:B
  DeletedManifestV2Mapping:C
  DeletedManifestV2Mapping:D
  DeletedManifestV2Mapping:E
  DeletedManifestV2Mapping:F
  DeletedManifestV2Mapping:G
  DeletedManifestV2Mapping:H
  $ test "$(grep -c '^DeletedManifestV2:' walker.nodes)" -gt 8

Chunked Walker loses nothing here, and scrub-all reaches the same nodes.

  $ walker_scrub_derived --chunk-size 3 | diff -u walker.nodes -
  $ walker_scrub_all --chunk-size 3 | diff -u walker.nodes -

With scrub-derived's families, every commit and family is validated, and the
union of each commit's additions is exactly the reference.

  $ strider "${strider_scrub_derived[@]}" --chunk-size 3 > derived.out
  $ grep -c ' status=validated$' derived.out
  16
  $ strider_nodes derived.out | diff -u walker.nodes -
  $ completed
  1
  $ LC_ALL=C sort derived.out > derived.sorted

Each commit reports the same additions when every commit is its own chunk, and
adding the unode family leaves the other families' output unchanged.

  $ strider "${strider_scrub_derived[@]}" --chunk-size 1 > single.out
  $ LC_ALL=C sort single.out | diff -u derived.sorted -
  $ completed
  1

  $ strider "${strider_scrub_all[@]}" --chunk-size 3 > all.out
  $ grep -v ' unode ' all.out | LC_ALL=C sort | diff -u derived.sorted -
  $ grep -c ' unode status=validated$' all.out
  8
  $ completed
  1

Resume after C, as a pass restarting from a checkpoint does. Strider audits only
D to H and reports the same additions for them as the full run. Walker also
loads nodes that commits below the range added; those are not compared.

  $ LOWER=$(( $(sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT id FROM commit_graph_edges WHERE cs_id = x'$C'") + 1 ))
  $ strider "${strider_scrub_derived[@]}" --chunk-size 3 --repo-lower-bound "$LOWER" > resume.out
  $ grep -E "^($D|$E|$F|$G|$H) " derived.sorted > expected-resume
  $ LC_ALL=C sort resume.out | diff -u expected-resume -
  $ completed
  1
