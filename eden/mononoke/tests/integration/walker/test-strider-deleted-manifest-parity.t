# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Create a deleted-manifest tree with multiple directory levels.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/nested/file2" "two"
  > # modify: A "dir2/file3" "three"
  > # delete: B "dir1/file1"
  > # delete: B "dir1/nested/file2"
  > # delete: B "dir2/file3"
  > # modify: B "live" "four"
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)

Walker's tree for one commit is every manifest node it reaches from that
commit's deleted-manifest mapping using only deleted-manifest tree edges.

  $ walker_tree() {
  >   mononoke_walker scrub -q \
  >     --walk-root "DeletedManifestV2Mapping:$1" \
  >     -I deep \
  >     -i derived_deleted_manifest \
  >     --include-output-node-type DeletedManifestV2Mapping \
  >     --include-output-node-type DeletedManifestV2 \
  >     --output-format Debug > "walker-$1.out" 2>&1
  >   grep '^\[INFO\] Walking edge types' "walker-$1.out" >&2
  >   sed -n \
  >     -e 's/^Node DeletedManifestV2(DeletedManifestV2Id(Blake2(\([0-9a-f]*\)))):.*/DeletedManifestV2:\1/p' \
  >     "walker-$1.out" | LC_ALL=C sort -u
  > }
  $ walker_tree "$A" > tree-a
  [INFO] Walking edge types [DeletedManifestV2MappingToRootDeletedManifestV2, DeletedManifestV2ToDeletedManifestV2Child]
  $ walker_tree "$B" > tree-b
  [INFO] Walking edge types [DeletedManifestV2MappingToRootDeletedManifestV2, DeletedManifestV2ToDeletedManifestV2Child]

B adds every node in its tree that is not in A's tree.

  $ LC_ALL=C comm -23 tree-b tree-a > added-b
  $ test "$(wc -l < added-b)" -gt 1

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
  >     "$@"
  > }

Strider audits only B, and validates exactly the manifest nodes B added,
after B's mapping.

  $ strider --changeset-id "$B" --family deleted-manifest --operation validate > strider.out 2> strider.err
  $ grep ' DeletedManifestV2:' strider.out | cut -d' ' -f3 | LC_ALL=C sort > strider-b
  $ diff -u added-b strider-b
  $ grep -v ' DeletedManifestV2:' strider.out | sed -e "s/$B/B/g"
  B deleted-manifest Mapping:deleted_manifest:B
  B deleted-manifest status=validated
  $ grep 'Strider completed' strider.err | grep -E 'status="?complete"?' | grep -E 'commits=1 ' > /dev/null

Remove one manifest node that B added. A range run over both commits still
validates A, and reports the exact node against B at the load stage.

  $ NODE=$(head -n 1 added-b | cut -d: -f2)
  $ BLOBS="$TESTTMP/blobstore/blobs"
  $ mv "$BLOBS/blob-repo0000.deletedmanifest2.blake2.$NODE" "$TESTTMP/damaged-node"
  $ strider --chunk-size 10 --family deleted-manifest > damaged.out 2> damaged.err
  [1]
  $ grep status= damaged.out | sed -e "s/$A/A/" -e "s/$B/B/" | LC_ALL=C sort
  A deleted-manifest status=validated
  B deleted-manifest status=incomplete
  $ grep 'Strider family incomplete' damaged.err | grep "changeset_id=.*$B" | grep -E 'failure_stage="?load"?' | grep -E 'failure_class="?missing"?' | grep "object=.*DeletedManifestV2:$NODE" > /dev/null
  $ grep 'Strider completed' damaged.err | grep -E 'status="?incomplete"?' | grep -E 'incomplete_commits=1 ' > /dev/null

Restore the node and remove A's mapping instead. A's own absent mapping is
counted and otherwise ignored, as Walker does. B is derived, so its parent's
mapping must have existed, and B reports it missing.

  $ mv "$TESTTMP/damaged-node" "$BLOBS/blob-repo0000.deletedmanifest2.blake2.$NODE"
  $ rm "$BLOBS/blob-repo0000.derived_root_deleted_manifest2.$A"
  $ strider --chunk-size 10 --family deleted-manifest > lost.out 2> lost.err
  [1]
  $ grep status= lost.out | sed -e "s/$A/A/" -e "s/$B/B/" | LC_ALL=C sort
  A deleted-manifest status=not_derived
  B deleted-manifest status=incomplete
  $ grep -c 'Strider family incomplete' lost.err
  1
  $ grep 'Strider family incomplete' lost.err | grep "changeset_id=.*$B" | grep -E 'failure_stage="?enumerate"?' | grep -E 'failure_class="?missing"?' | grep "object=.*Mapping:deleted_manifest:$A" > /dev/null
  $ grep 'Strider completed' lost.err | grep -E 'incomplete_commits=1 ' | grep -E 'not_derived_ignored=1 ' > /dev/null
