# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Over full history, the union of what each commit added must contain the
Changeset, DeletedManifestV2Mapping and DeletedManifestV2 nodes that Walker
visits in the deployed validate-all job, with no check failures. validate-all
is a shallow walk from bookmark master over every type except Blame; tailing,
sharding and replica arguments are omitted. Strider instead validates what
each commit added, over an SQL-ID range. Hg file nodes and their
HgLinkNodePopulated check are not covered: Strider has no family that reaches
them yet.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  >     E
  >    / \
  >   C   D
  >    \ /
  >     B
  >     |
  >     A
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # delete: B "dir1/file1"
  > # modify: C "dir2/file2" "two-c"
  > # delete: D "dir1/sub/file3"
  > # modify: D "live" "live-d"
  > # modify: E "dir2/file2" "two-c"
  > # delete: E "dir1/sub/file3"
  > # modify: E "live" "live-e"
  > # bookmark: E master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)
  E=* (glob)

  $ names() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g" -e "s/$E/E/g"
  > }
  $ identities() {
  >   sed -nE \
  >     -e 's/^Node Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/Changeset:\1/p' \
  >     -e 's/^Node DeletedManifestV2Mapping\(ChangesetId\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2Mapping:\1/p' \
  >     -e 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2:\1/p' \
  >     -e 's/^(Changeset|DeletedManifestV2Mapping|DeletedManifestV2):([0-9a-f]+)$/\1:\2/p' \
  >   | names | LC_ALL=C sort -u
  > }
  $ walker_graph_args=(-I shallow -i all -x Blame --bookmark master)
  $ walker_identities() {
  >   mononoke_walker scrub -q "${walker_graph_args[@]}" \
  >     --include-output-node-type Changeset \
  >     --include-output-node-type DeletedManifestV2Mapping \
  >     --include-output-node-type DeletedManifestV2 \
  >     --output-format Debug
  > }
  $ walker_validate() {
  >   mononoke_walker validate -q "${walker_graph_args[@]}"
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
  >     --operation validate --family core --family deleted-manifest \
  >     --chunk-size 2 \
  >     "$@"
  > }
  $ strider_nodes() {
  >   grep -v ' status=' "$1" | cut -d' ' -f3 \
  >     | sed -e 's/^Mapping:deleted_manifest:/DeletedManifestV2Mapping:/' | identities
  > }

Walker's scrub and validate walks use the same edges, so scrub output
supplies the identities that validate visits.

  $ walker_identities > walker-scrub.out 2>&1
  $ walker_validate > walker-validate.out 2>&1
  $ grep 'Walking edge types' walker-validate.out | diff - <(grep 'Walking edge types' walker-scrub.out)

The shallow walk reaches master's changeset and deleted manifest, but not
history: no parent changesets and no linked changesets.

  $ identities < walker-scrub.out > walker.nodes
  $ grep -E '^(Changeset|DeletedManifestV2Mapping):' walker.nodes
  Changeset:E
  DeletedManifestV2Mapping:E
  $ grep -c '^DeletedManifestV2:' walker.nodes
  5

Strider validates every commit and family with no check failures, and the
union of each commit's additions covers master's tree.

  $ strider > strider.out 2> strider.err
  $ grep -c ' status=validated$' strider.out
  10
  $ strider_nodes strider.out | LC_ALL=C comm -23 walker.nodes -
  $ grep 'Strider completed' strider.err | grep -E 'status="?complete"?' | grep -c 'validation_failed=0'
  1
