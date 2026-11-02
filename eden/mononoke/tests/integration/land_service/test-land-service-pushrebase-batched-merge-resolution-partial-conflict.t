# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Verify that batched pushrebase rejects when at least one file has a
# true conflict, even if other files could be cleanly merged.
#
# If file A has non-overlapping edits (mergeable) but file B has
# overlapping edits (true conflict), the entire push must be rejected.
# Partial merge is not supported — all files must be resolvable.

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

Create a client and set up two files
  $ hg clone -q mono:repo client1
  $ cd $TESTTMP/client1
  $ cat > mergeable.txt << 'EOF'
  > m_line1
  > m_line2
  > m_line3
  > m_line4
  > m_line5
  > EOF
  $ cat > conflict.txt << 'EOF'
  > c_line1
  > c_line2
  > c_line3
  > EOF
  $ hg addremove -q
  $ hg ci -m "add mergeable.txt and conflict.txt"
  $ quiet hg push --to master_bookmark

Server edits: line 1 of mergeable.txt (non-overlapping) and line 2 of conflict.txt
  $ quiet hg pull
  $ quiet hg up remote/master_bookmark
  $ cat > mergeable.txt << 'EOF'
  > SERVER_M_LINE1
  > m_line2
  > m_line3
  > m_line4
  > m_line5
  > EOF
  $ cat > conflict.txt << 'EOF'
  > c_line1
  > SERVER_EDIT
  > c_line3
  > EOF
  $ hg ci -m "server edits"
  $ quiet hg push --to master_bookmark

Client edits: line 5 of mergeable.txt (non-overlapping) and line 2 of conflict.txt (overlapping!)
  $ quiet hg up .~1
  $ cat > mergeable.txt << 'EOF'
  > m_line1
  > m_line2
  > m_line3
  > m_line4
  > CLIENT_M_LINE5
  > EOF
  $ cat > conflict.txt << 'EOF'
  > c_line1
  > CLIENT_EDIT
  > c_line3
  > EOF
  $ hg ci -m "client edits"

Pushrebase should FAIL — conflict.txt has overlapping edits
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue * for upload (glob)
  edenapi: uploaded * (glob)
  edenapi: queue * tree* for upload (glob)
  edenapi: uploaded * tree* (glob)
  edenapi: uploaded 1 changeset
  pushrebasing stack (*, *] (1 commit) to remote bookmark master_bookmark (glob)
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("conflict.txt"), right: MPath("conflict.txt") }]
  [255]
