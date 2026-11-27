# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

A sharded job selects the same families for every repository, and Walker walks
only the derived types a repository enables. Strider drops a selected family
whose derived type the repository has not enabled, logs it as unavailable, and
audits the other families. This repository enables only fsnodes, so it has no
unodes.

  $ ENABLED_DERIVED_DATA="fsnodes" setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B
  > # modify: A "file" "a"
  > # modify: B "file" "b"
  > # bookmark: B master
  > EOF
  A=* (glob)
  B=* (glob)

  $ render_mononoke_config
  $ GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >   "${CACHE_ARGS[@]}" \
  >   "${COMMON_ARGS[@]}" \
  >   --repo-id "$REPOID" \
  >   --config-tier "$MONONOKE_CONFIG_TIER" \
  >   --tracing-test-format \
  >   --max-concurrency 4 \
  >   --max-loads-per-commit 4 \
  >   --print-visited-nodes \
  >   --family core --family unode \
  >   --chunk-size 2 > strider.out 2> strider.err
  [1]

Only core runs, on both commits. The unode family was asked for but could not
be checked, so the run is incomplete and exits 1.

  $ grep ' status=' strider.out | sed -e "s/$A/A/" -e "s/$B/B/" | LC_ALL=C sort
  A core status=validated
  B core status=validated
  $ grep 'Strider family unavailable' strider.err | sed -nE 's/.* family="?([a-z-]+)"?.*/\1/p'
  unode
  $ grep 'Strider completed' strider.err | grep -E 'status="?incomplete"?' | grep -cE ' unavailable_families=1 '
  1
