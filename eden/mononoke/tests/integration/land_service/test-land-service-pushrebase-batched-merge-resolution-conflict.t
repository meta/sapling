# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Verify that batched pushrebase merge resolution rejects true content
# conflicts even with merge resolution enabled.
#
# When both sides modify the same line of a file, the 3-way merge detects
# overlapping edits and the push is rejected with a PushrebaseConflict
# error through the batched pushrebase (land service) path.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ enable rebase pushrebase commitcloud
  $ setconfig push.edenapi=true

Setup repo
  $ SCUBA_LOGGING_PATH="$TESTTMP/scuba.json"
  $ setup_common_config
  $ cd $TESTTMP
  $ testtool_drawdag -R repo << EOF
  >   B
  >   |
  >   A
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)

Enable batching and merge resolution
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:land_service_batching_enabled": true,
  >     "scm/mononoke:pushrebase_enable_merge_resolution": true,
  >     "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes": true
  >   },
  >   "ints": {
  >     "scm/mononoke:land_service_batch_time_ms": 5000,
  >     "scm/mononoke:land_service_batch_max_commits": 2,
  >     "scm/mononoke:pushrebase_max_merge_conflicts": 10,
  >     "scm/mononoke:pushrebase_max_merge_file_size": 10485760
  >   }
  > }
  > EOF

Start services
  $ start_and_wait_for_land_service --scuba-log-file "$TESTTMP/land_service_scuba.json"
  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" << EOF
  > [pushrebase.remote_mode]
  > remote_land_service = { host_port = "$(land_service_address)" }
  > EOF
  $ start_and_wait_for_mononoke_server --scuba-log-file "$SCUBA_LOGGING_PATH"

Create a client and set up a base file
  $ hg clone -q mono:repo client1
  $ cd $TESTTMP/client1
  $ cat > conflict.txt << 'EOF'
  > line1
  > line2
  > line3
  > EOF
  $ hg addremove -q
  $ hg ci -m "add conflict.txt"
  $ quiet hg push --to master_bookmark

Server edits line 2
  $ quiet hg pull
  $ quiet hg up remote/master_bookmark
  $ cat > conflict.txt << 'EOF'
  > line1
  > SERVER_EDIT
  > line3
  > EOF
  $ hg ci -m "server: edit line 2"
  $ quiet hg push --to master_bookmark

Client also edits line 2 differently (from pre-server base)
  $ quiet hg up .~1
  $ cat > conflict.txt << 'EOF'
  > line1
  > CLIENT_EDIT
  > line3
  > EOF
  $ hg ci -m "client: edit line 2"

Pushrebase should FAIL — overlapping edits are a true conflict
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue * for upload (glob)
  edenapi: uploaded * (glob)
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  pushrebasing stack (*, *] (1 commit) to remote bookmark master_bookmark (glob)
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("conflict.txt"), right: MPath("conflict.txt") }]
  [255]
