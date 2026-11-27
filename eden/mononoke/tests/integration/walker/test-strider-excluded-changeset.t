# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

A known-corrupt changeset would fail every run. Walker's jobs exclude it by
exact node. Strider excludes it with `--exclude-changeset`: since nobody knows
what else of that commit is damaged, every family reports the commit excluded
and reads nothing for it, and the run completes.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B
  > # modify: A "dir/file" "one"
  > # modify: A "live" "live"
  > # delete: B "dir/file"
  > # bookmark: B master
  > EOF
  A=* (glob)
  B=* (glob)

  $ printf 'garbage' > "$TESTTMP/blobstore/blobs/blob-repo0000.changeset.blake2.$B"
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
  $ outcomes() {
  >   grep ' status=' strider.out | sed -e "s/$A/A/g" -e "s/$B/B/g" | LC_ALL=C sort
  >   grep 'Strider family incomplete' strider.err | sed -nE \
  >     's/.*changeset_id="?([0-9a-f]+)"? family="?([a-z-]+)"? failure_stage="?([a-z]+)"? failure_class="?([a-z_]+)"? object="?([^" ]+)"?.*/\1 \2 \3 \4 \5/p' \
  >   | sed -e "s/$A/A/g" -e "s/$B/B/g"
  > }

Without the exclusion, B's core fails and the run is incomplete.

  $ strider
  [1]
  $ outcomes
  A core status=validated
  A deleted-manifest status=validated
  B core status=incomplete
  B deleted-manifest status=validated
  B core load unreadable Changeset:B

With B excluded, every family reports B excluded, and the run completes.

  $ strider --exclude-changeset "$B"
  $ outcomes
  A core status=validated
  A deleted-manifest status=validated
  B core status=excluded
  B deleted-manifest status=excluded
  $ grep 'Strider completed' strider.err | grep -cE 'status="?complete"?'
  1
