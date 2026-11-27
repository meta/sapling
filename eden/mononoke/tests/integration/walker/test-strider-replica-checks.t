# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

With `--check-replicas`, Strider reads every copy of each key from a
two-store multiplex, as the deployed Walker jobs do in `ReportOnly` scrub
mode. A copy missing for longer than the 120-second grace period is a replica
gap: it is counted, logged to Scuba and reported against the commit and object
whose load read it. A copy missing within the grace period is not reported.
Stores holding different values make the load unreadable. Nothing is ever
repaired.

  $ MULTIPLEXED=1 setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B-C
  > # modify: A "old" "an old blob"
  > # modify: B "recent" "a recent blob"
  > # modify: C "differs" "a differing blob"
  > # bookmark: C master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

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
  >     --chunk-size 10 \
  >     "$@" > strider.out 2> strider.err
  > }
  $ names() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" \
  >     -e "s/$OLD/OLD/g" -e "s/$RECENT/RECENT/g" -e "s/$DIFFERS/DIFFERS/g"
  > }

Each commit adds one file. A healthy multiplex has no gaps.

  $ strider --check-replicas
  $ content() {
  >   sed -nE "s/^$1 core FileContent:([0-9a-f]+)$/\1/p" strider.out
  > }
  $ OLD=$(content "$A")
  $ RECENT=$(content "$B")
  $ DIFFERS=$(content "$C")
  $ test -n "$OLD" && test -n "$RECENT" && test -n "$DIFFERS"
  $ grep 'Strider completed' strider.err | grep -E 'status="?complete"?' | grep -cE 'replica_gaps=0 replica_gaps_unattributed=0'
  1

Store 0 loses A's file, whose surviving copy is an hour old, and B's file,
whose surviving copy was just written. Store 0's copy of C's file holds
another blob's value.

  $ STORE0="$TESTTMP/blobstore/0/blobs"
  $ STORE1="$TESTTMP/blobstore/1/blobs"
  $ rm "$STORE0/blob-repo0000.content.blake2.$OLD"
  $ touch -d '1 hour ago' "$STORE1/blob-repo0000.content.blake2.$OLD"
  $ rm "$STORE0/blob-repo0000.content.blake2.$RECENT"
  $ cp "$STORE1/blob-repo0000.content.blake2.$RECENT" "$STORE0/blob-repo0000.content.blake2.$DIFFERS"

B's surviving copy is touched so that it is seconds old when Strider reads it.
Both stores and the write-ahead log are recorded before and after the run.

  $ snapshot() {
  >   (cd "$TESTTMP/blobstore" && find . -type f | LC_ALL=C sort | while read -r f; do
  >     echo "$f $(stat -c %Y "$f") $(sha1sum < "$f")"
  >   done)
  >   read_blobstore_wal_queue_size
  > }
  $ touch "$STORE1/blob-repo0000.content.blake2.$RECENT"
  $ snapshot > before

  $ strider --check-replicas --scuba-log-file scuba.json
  [1]

The differing values fail C's core family as unreadable.

  $ grep ' status=' strider.out | grep -v ' status=validated$' | names
  C core status=incomplete
  $ grep 'Strider family incomplete' strider.err | sed -nE \
  >   's/.*changeset_id="?([0-9a-f]+)"? family="?([a-z-]+)"? failure_stage="?([a-z]+)"? failure_class="?([a-z_]+)"? object="?([^" ]+)"?.*/\1 \2 \3 \4 \5/p' \
  >   | names
  C core load unreadable FileContent:DIFFERS

Only the old missing copy is a replica gap, reported against the commit and
object that read it, and logged to Scuba with its store.

  $ grep 'Strider replica gap' strider.err | sed -nE \
  >   's/.*changeset_id="?([0-9a-f]+)"? object="?([^" ]+)"? store="?([0-9]+)"? key="?([^" ]+)"?.*/\1 \2 \3 \4/p' \
  >   | names
  A FileContent:OLD 0 repo0000.content.blake2.OLD
  $ grep 'Strider completed' strider.err | grep -E 'status="?incomplete"?' | grep -cE 'replica_gaps=1 replica_gaps_unattributed=0'
  1
  $ jq -r '.int * .normal | [ .check_fail, .check_type, .node_key, .repo, .blobstore_id ] | @csv' < scuba.json | names
  1,"scrub_repair","repo0000.content.blake2.OLD","repo",0

Nothing was written: no store gained, lost or changed a blob, and the
write-ahead log is unchanged.

  $ snapshot > after
  $ diff -u before after
