# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Absent, missing or unreadable data in history. Walker shows
where each damaged node belongs. Strider must report the failure against the
commit that added the node, keep the other commits validated, and exit
nonzero. See test-strider-chunked-deep-history.t for the Walker and Strider
arguments.

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
  > # bookmark: D master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)

  $ BLOBS="$TESTTMP/blobstore/blobs"
  $ names() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g"
  > }
  $ identities() {
  >   sed -nE \
  >     -e 's/^Node Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/Changeset:\1/p' \
  >     -e 's/^Node DeletedManifestV2Mapping\(ChangesetId\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2Mapping:\1/p' \
  >     -e 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2:\1/p' \
  >     -e 's/^(Changeset|DeletedManifestV2Mapping|DeletedManifestV2):([0-9a-f]+)$/\1:\2/p' \
  >   | names | LC_ALL=C sort -u
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
  >     --chunk-size 2 \
  >     --include-output-node-type Changeset \
  >     --include-output-node-type DeletedManifestV2Mapping \
  >     --include-output-node-type DeletedManifestV2 \
  >     --output-format Debug > walker.out 2>&1
  > }
  $ walker_failure() {
  >   local class=unknown
  >   grep -q ' is missing$' walker.out && class=missing_target
  >   grep -q 'error while deserializing' walker.out && class=unreadable_target
  >   grep -m1 'Could not step to' walker.out | sed -nE \
  >     -e "s/.*target: Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/$class Changeset:\1/p" \
  >     -e "s/.*target: DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/$class DeletedManifestV2:\1/p" \
  >   | names
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
  >     --chunk-size 2 \
  >     "$@" > strider.out 2> strider.err
  > }
  $ failures() {
  >   grep 'Strider family incomplete' strider.err | sed -nE \
  >     's/.*changeset_id="?([0-9a-f]+)"? family="?([a-z-]+)"? failure_stage="?([a-z]+)"? failure_class="?([a-z_]+)"? object="?([^" ]+)"?.*/\1 \2 \3 \4 \5/p' \
  >   | names | LC_ALL=C sort
  > }

The number of commit and family results, those not validated, and each
failure as commit, family, stage, class and object.

  $ outcomes() {
  >   grep -c ' status=' strider.out
  >   grep ' status=' strider.out | grep -v ' status=validated$' | names | LC_ALL=C sort
  >   failures
  > }

  $ walker_scrub_derived
  $ identities < walker.out > walker.nodes
  $ grep -E '^(Changeset|DeletedManifestV2Mapping):' walker.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  Changeset:D
  DeletedManifestV2Mapping:A
  DeletedManifestV2Mapping:B
  DeletedManifestV2Mapping:C
  DeletedManifestV2Mapping:D

A directory node deleted in B, with children, is shared by the trees of B, C
and D.

  $ SHARED=$(grep -F "linknode: Some(ChangesetId(Blake2($B))), subentries: Terminal { values: {[" walker.out \
  >   | sed -nE 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/\1/p' | head -n 1)
  $ test -n "$SHARED"

An absent mapping in mid-history ends that branch normally. Walker completes
and still reaches B's deleted-manifest nodes through C and D. Strider counts
B's own absent mapping and otherwise ignores it, as Walker does, but C was
derived from B's mapping, so C reports it missing.

  $ mv "$BLOBS/blob-repo0000.derived_root_deleted_manifest2.$B" "$TESTTMP/mapping-B"
  $ walker_scrub_derived
  $ identities < walker.out > walker-absent.nodes
  $ grep -c "^DeletedManifestV2:$SHARED$" walker-absent.nodes
  1
  $ strider
  [1]
  $ outcomes
  8
  B deleted-manifest status=not_derived
  C deleted-manifest status=incomplete
  C deleted-manifest enumerate missing Mapping:deleted_manifest:B
  $ mv "$TESTTMP/mapping-B" "$BLOBS/blob-repo0000.derived_root_deleted_manifest2.$B"

A missing shared deleted-manifest node is fatal. Strider reports it against B,
which added it.

  $ mv "$BLOBS/blob-repo0000.deletedmanifest2.blake2.$SHARED" "$TESTTMP/shared-dm"
  $ walker_scrub_derived
  [1]
  $ walker_failure | sed "s/$SHARED/SHARED/"
  missing_target DeletedManifestV2:SHARED
  $ strider
  [1]
  $ outcomes | sed "s/$SHARED/SHARED/"
  8
  B deleted-manifest status=incomplete
  B deleted-manifest load missing DeletedManifestV2:SHARED

The same node present but undecodable is an unreadable target.

  $ printf 'garbage' > "$BLOBS/blob-repo0000.deletedmanifest2.blake2.$SHARED"
  $ walker_scrub_derived
  [1]
  $ walker_failure | sed "s/$SHARED/SHARED/"
  unreadable_target DeletedManifestV2:SHARED
  $ strider
  [1]
  $ outcomes | sed "s/$SHARED/SHARED/"
  8
  B deleted-manifest status=incomplete
  B deleted-manifest load unreadable DeletedManifestV2:SHARED
  $ mv "$TESTTMP/shared-dm" "$BLOBS/blob-repo0000.deletedmanifest2.blake2.$SHARED"

A missing changeset, reached as a root and through B's parent edge, is fatal.
Strider reports it against A's core family only: the other families take A's
parents from the commit graph, so A's deleted manifest is still validated.

  $ mv "$BLOBS/blob-repo0000.changeset.blake2.$A" "$TESTTMP/changeset-A"
  $ walker_scrub_derived
  [1]
  $ walker_failure
  missing_target Changeset:A
  $ strider
  [1]
  $ outcomes
  8
  A core status=incomplete
  A core load missing Changeset:A
  $ mv "$TESTTMP/changeset-A" "$BLOBS/blob-repo0000.changeset.blake2.$A"
