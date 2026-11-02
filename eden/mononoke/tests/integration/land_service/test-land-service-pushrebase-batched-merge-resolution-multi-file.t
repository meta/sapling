# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Verify that batched pushrebase merge resolution handles multiple files
# with non-overlapping edits.
#
# When server and client each modify different regions of multiple files,
# all files should be correctly merged through the batched pushrebase
# (land service) path.

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

Create a client and set up two shared files
  $ hg clone -q mono:repo client1
  $ cd $TESTTMP/client1
  $ cat > alpha.txt << 'EOF'
  > a_line1
  > a_line2
  > a_line3
  > a_line4
  > a_line5
  > EOF
  $ cat > beta.txt << 'EOF'
  > b_line1
  > b_line2
  > b_line3
  > b_line4
  > b_line5
  > EOF
  $ hg addremove -q
  $ hg ci -m "add alpha.txt and beta.txt"
  $ quiet hg push --to master_bookmark

Server-side commit: modify first line of both files
  $ quiet hg pull
  $ quiet hg up remote/master_bookmark
  $ cat > alpha.txt << 'EOF'
  > SERVER_ALPHA_LINE1
  > a_line2
  > a_line3
  > a_line4
  > a_line5
  > EOF
  $ cat > beta.txt << 'EOF'
  > SERVER_BETA_LINE1
  > b_line2
  > b_line3
  > b_line4
  > b_line5
  > EOF
  $ hg ci -m "server: edit line 1 of both files"
  $ quiet hg push --to master_bookmark

Client commit (from pre-server base): modify last line of both files
  $ quiet hg up .~1
  $ cat > alpha.txt << 'EOF'
  > a_line1
  > a_line2
  > a_line3
  > a_line4
  > CLIENT_ALPHA_LINE5
  > EOF
  $ cat > beta.txt << 'EOF'
  > b_line1
  > b_line2
  > b_line3
  > b_line4
  > CLIENT_BETA_LINE5
  > EOF
  $ hg ci -m "client: edit line 5 of both files"

Pushrebase should succeed — both files merged independently
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue * for upload (glob)
  edenapi: uploaded * (glob)
  edenapi: queue * tree* for upload (glob)
  edenapi: uploaded * tree* (glob)
  edenapi: uploaded 1 changeset
  pushrebasing stack (*, *] (1 commit) to remote bookmark master_bookmark (glob)
  * files updated, 0 files merged, 0 files removed, 0 files unresolved (glob)
  updated remote bookmark master_bookmark to * (glob)

Verify both files have merged content
  $ quiet hg pull
  $ quiet hg up remote/master_bookmark
  $ cat alpha.txt
  SERVER_ALPHA_LINE1
  a_line2
  a_line3
  a_line4
  CLIENT_ALPHA_LINE5
  $ cat beta.txt
  SERVER_BETA_LINE1
  b_line2
  b_line3
  b_line4
  CLIENT_BETA_LINE5
