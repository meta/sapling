# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Strider refuses command lines that could report a run complete without having
read what it should.

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
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4 \
  >     --family core \
  >     "$@" > strider.out 2> strider.err
  > }

The whole range, with the tests' local cache, completes.

  $ strider "${CACHE_ARGS[@]}" --chunk-size 2
  $ grep 'Strider completed' strider.err | grep -oE ' commits="?[0-9]+' | tr -d '" '
  commits=2

A shared cache could answer a read for a blob that storage has lost, so it is
refused. Open-source builds, which have no cachelib, abort when they try to
start one.

  $ strider --cache-mode=enabled --cache-size-gb=1 --cachelib-disable-cacheadmin --chunk-size 2 2>/dev/null || echo refused
  refused
  $ grep -qE 'shared cache could answer for blobs that storage has lost|cachelib works only for fbcode builds' strider.err && ! grep -q 'Strider completed' strider.err && echo "no audit ran"
  no audit ran

An empty range checks nothing.

  $ strider "${CACHE_ARGS[@]}" --chunk-size 2 --repo-lower-bound 2 --repo-upper-bound 1
  [1]
  $ grep -o 'SQL-ID range .* is empty' strider.err
  SQL-ID range 2..1 is empty

An upper bound past the replica's newest SQL ID would silently skip the commits
the replica has not received yet.

  $ strider "${CACHE_ARGS[@]}" --chunk-size 2 --repo-upper-bound 100
  [1]
  $ grep -o 'upper bound 100 is past .*' strider.err
  upper bound 100 is past the replica's exclusive upper bound, 3
