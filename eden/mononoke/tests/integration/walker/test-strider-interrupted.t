# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

A run interrupted with Ctrl-C while it checks commits stops, logs its summary
so far, and exits 1, since not everything was checked.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all <<'EOF'
  > A-B-C-D-E-F-G-H-I-J-K-L-M-N-O-P-Q-R-S-T
  > # bookmark: T master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)
  E=* (glob)
  F=* (glob)
  G=* (glob)
  H=* (glob)
  I=* (glob)
  J=* (glob)
  K=* (glob)
  L=* (glob)
  M=* (glob)
  N=* (glob)
  O=* (glob)
  P=* (glob)
  Q=* (glob)
  R=* (glob)
  S=* (glob)
  T=* (glob)

Reads are limited to one a second, so the run is still checking commits when
`timeout` stops it. `--preserve-status` keeps Strider's own exit status.

  $ strider() {
  >   render_mononoke_config
  >   GLOG_minloglevel=5 timeout --preserve-status "$@" "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --max-concurrency 1 \
  >     --max-loads-per-commit 1 \
  >     --family core \
  >     --chunk-size 10 \
  >     --blobstore-read-qps 1 \
  >     --progress-interval-secs 1 \
  >     > strider.out 2> strider.err
  > }
  $ stopped() {
  >   grep 'Strider completed' strider.err | grep -oE '(status|stopped_by_signal)="?[a-z]+"?' | tr -d '"'
  >   grep 'Strider completed' strider.err | grep -oE ' commits="?[0-9]+' | tr -d '" ' | awk -F= '$2 < 20 { print "not every commit checked" }'
  >   grep -c 'Strider was stopped by a signal' strider.err
  > }

SIGINT, as Ctrl-C sends:

  $ strider -s INT 10
  [1]
  $ stopped
  status=incomplete
  stopped_by_signal=true
  not every commit checked
  1

SIGTERM, as `timeout` and job schedulers send:

  $ strider -s TERM 10
  [1]
  $ stopped
  status=incomplete
  stopped_by_signal=true
  not every commit checked
  1

While it runs, the log gives its progress every `--progress-interval-secs`:
commits checked of the total, and problems so far.

  $ grep 'Strider progress' strider.err | head -1 | grep -oE ' total_commits="?[0-9]+' | tr -d '" '
  total_commits=20
