# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for bookmark service scope with Mononoke server
# This test validates that Mononoke sends HG_ONLY scope by default
# and can be overridden to send ALL_KINDS scope with a flag.

setup
  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ export ENABLE_BOOKMARK_CACHE=1

  $ REPONAME="repo" setup_common_config

  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C
  > # bookmark: C master_bookmark
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Start bookmark service
  $ start_and_wait_for_bookmark_service

Start Mononoke with remote bookmark cache (default scope)
  $ start_and_wait_for_mononoke_server_with_client_certs \
  >   --use-remote-bookmark-cache true \
  >   --remote-bookmark-cache-host-port localhost:$BOOKMARK_SERVICE_PORT

Clone and perform bookmark lookup
  $ setconfig pull.httphashprefix=1 pull.use-commit-graph=true clone.use-rust=true
  $ hg clone -q mono:repo client --noupdate
  $ cd client
  $ hg log -r "master_bookmark" -T '{desc}\n'
  C

Verify Mononoke sent HG_ONLY scope by default
  $ jq -r 'select(.normal.method=="get" and .normal.bookmark=="master_bookmark") | .normal.derived_data_set' \
  >   "$TESTTMP/bookmark-service.json" | tail -1
  Some(DerivedDataSet::HG_ONLY)

