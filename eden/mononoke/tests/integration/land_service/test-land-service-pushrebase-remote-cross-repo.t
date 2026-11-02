# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

  $ SCUBA_LOGGING_PATH="$TESTTMP/scuba.json"
  $ setconfig push.edenapi=true
  $ large_small_config
  $ start_and_wait_for_land_service --scuba-log-file "$SCUBA_LOGGING_PATH"
  $ cat >> repos/small-mon/server.toml <<EOF
  > [pushrebase.remote_mode]
  > remote_land_service = { host_port = "$(land_service_address)" }
  > EOF
  $ cat >> repos/large-mon/server.toml <<EOF
  > [pushrebase.remote_mode]
  > remote_land_service = { host_port = "$(land_service_address)" }
  > EOF
  $ large_small_setup
  Adding synced mapping entry
  $ setup_configerator_configs
  $ enable_pushredirect 1
  $ start_large_small_repo --scuba-log-file "$SCUBA_LOGGING_PATH"
  Starting Mononoke server
  $ init_local_large_small_clones

Normal pushrebase with one commit
  $ cd "$TESTTMP/small-hg-client"
  $ hg up -q master_bookmark
  $ echo 2 > 2 && hg addremove -q && hg ci -q -m newcommit
  $ hg push -r . --to master_bookmark
  pushing rev ce81c7d38286 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  pushrebasing stack (11f848659bfc, ce81c7d38286] (1 commit) to remote bookmark master_bookmark
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master_bookmark to ce81c7d38286
-- newcommit was correctly pushed to master_bookmark
  $ log -r master_bookmark
  @  newcommit [public;rev=2;ce81c7d38286] remote/master_bookmark
  │
  ~

-- newcommit is also present in the large repo (after a pull)
  $ cd "$TESTTMP"/large-hg-client
  $ log -r master_bookmark
  @  first post-move commit [public;rev=2;bfcfb674663c] remote/master_bookmark
  │
  ~
  $ hg pull -q
  $ log -r master_bookmark
  o  newcommit [public;rev=3;819e91b238b7] remote/master_bookmark
  │
  ~
-- compare the working copies
  $ verify_wc $(hg log -r master_bookmark -T '{node}')
-- check there were no errors
  $ cat $SCUBA_LOGGING_PATH | jq 'select(.normal.log_tag=="failed_remote_pushrebase")'
-- check it actually used Land service
  $ cat $SCUBA_LOGGING_PATH | jq 'select(.normal.method=="land_changesets" and .normal.log_tag=="Request complete") | .normal.status'
  "SUCCESS"
-- check the target bookmark is logged alongside the outcome
  $ cat $SCUBA_LOGGING_PATH | jq 'select(.normal.method=="land_changesets" and .normal.log_tag=="Request complete") | .normal.bookmark'
  "master_bookmark"
