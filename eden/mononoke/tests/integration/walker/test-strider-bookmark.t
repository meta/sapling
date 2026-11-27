# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

`--bookmark` audits what the bookmarked changeset added, as `--changeset-id`
does for that changeset. Objects are printed in the order their loads finish,
so both outputs are sorted.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B
  > # modify: A "file" "a"
  > # modify: B "file" "b"
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)

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
  >     "$@" > strider.out 2> strider.err
  > }
  $ strider --changeset-id "$B"
  $ LC_ALL=C sort strider.out > by-id.out
  $ strider --bookmark master_bookmark
  $ LC_ALL=C sort strider.out | diff by-id.out -
  $ grep ' status=' strider.out | sed -e "s/$B/B/" | LC_ALL=C sort
  B core status=validated
  B deleted-manifest status=validated

A bookmark that does not exist is an error.

  $ strider --bookmark missing
  [1]
  $ grep -o 'bookmark missing does not exist' strider.err
  bookmark missing does not exist
