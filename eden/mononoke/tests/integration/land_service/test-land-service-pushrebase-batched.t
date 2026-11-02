# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ enable rebase pushrebase commitcloud
  $ setconfig push.edenapi=true
  $ function stablelog() {
  >   hg log -G -T "{desc} {phase} {remotenames}" --rev '::remote/master_bookmark' | sed 's/^[ \t]*$/$/'
  > }

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

Enable batching in land service
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:land_service_batching_enabled": true
  >   },
  >   "ints": {
  >     "scm/mononoke:land_service_batch_time_ms": 5000,
  >     "scm/mononoke:land_service_batch_max_commits": 2
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

Prepare two clients with commits touching different files
  $ hg clone -q mono:repo client1
  $ hg clone -q mono:repo client2
  $ cd $TESTTMP/client1
  $ echo "content1" > file1.txt
  $ hg addremove -q
  $ hg commit -q -m "C1"
  $ cd $TESTTMP/client2
  $ echo "content2" > file2.txt
  $ hg addremove -q
  $ hg commit -q -m "C2"

Prepare client3 for conflict test (based on B, before any concurrent pushes)
  $ cd $TESTTMP
  $ hg clone -q mono:repo client3
  $ cd client3
  $ echo "conflict" > file1.txt
  $ hg addremove -q
  $ hg commit -q -m "Conflict"

Push concurrently from both clients to trigger batching
  $ cd $TESTTMP/client1
  $ quiet hg push --to master_bookmark &
  $ PUSH1_PID=$!
  $ cd $TESTTMP/client2
  $ quiet hg push --to master_bookmark &
  $ PUSH2_PID=$!
  $ wait $PUSH1_PID
  $ wait $PUSH2_PID

Verify both commits landed on master_bookmark
  $ cd $TESTTMP/client1
  $ quiet hg pull
  $ hg log -r '::remote/master_bookmark' -T '{desc}\n' | sort
  A
  B
  C1
  C2

Verify request and batch counts are attributed to the repo and bookmark
  $ jq -c 'select(.normal.log_tag == "Batch received") | [.normal.repo_name, .normal.bookmark, .int.changesets_count, .int.batch_size, .int.batch_total_commits]' "$TESTTMP/land_service_scuba.json" | sort
  ["repo","master_bookmark",1,2,2]
  ["repo","master_bookmark",1,2,2]

Verify successful batch members log their individual and shared commit counts
  $ jq -c 'select(.normal.log_tag == "batched_pushrebase_request_complete") | [.normal.repo_name, .normal.bookmark, .int.rebased_changesets, .int.batch_size, .int.batch_total_commits]' "$TESTTMP/land_service_scuba.json" | sort
  ["repo","master_bookmark",1,2,2]
  ["repo","master_bookmark",1,2,2]

Verify request completion includes input and output commit counts
  $ jq -c 'select(.normal.log_tag == "Request complete") | [.normal.repo_name, .normal.bookmark, .int.changesets_count, .int.rebased_changesets]' "$TESTTMP/land_service_scuba.json" | sort
  ["repo","master_bookmark",1,1]
  ["repo","master_bookmark",1,1]

Verify both kinds of completion event identify the same committed batch
  $ jq -s '[.[] | select(.normal.log_tag == "Request complete" or .normal.log_tag == "batched_pushrebase_request_complete") | .int.bookmark_log_id] | length == 4 and (unique | length) == 1 and all(.[]; . > 0)' "$TESTTMP/land_service_scuba.json"
  true

Test conflict detection through batched path
  $ cd $TESTTMP/client3
  $ hg push --to master_bookmark
  pushing rev * to destination https://localhost:*/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  pushrebasing stack (*, *] (1 commit) to remote bookmark master_bookmark (glob)
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("file1.txt"), right: MPath("file1.txt") }]
  [255]

Test push after a batch completes (batcher loop continues)
  $ cd $TESTTMP/client1
  $ quiet hg pull
  $ echo "post_batch" > file3.txt
  $ hg addremove -q
  $ hg commit -q -m "PostBatch"
  $ quiet hg push --to master_bookmark
  $ quiet hg pull
  $ hg log -r '::remote/master_bookmark' -T '{desc}\n' | sort
  A
  B
  C1
  C2
  PostBatch

Test multi-commit stack push through batched path
  $ cd $TESTTMP/client2
  $ quiet hg pull
  $ quiet hg up remote/master_bookmark
  $ echo "stack1" > stack1.txt
  $ hg addremove -q
  $ hg commit -q -m "Stack1"
  $ echo "stack2" > stack2.txt
  $ hg addremove -q
  $ hg commit -q -m "Stack2"
  $ quiet hg push --to master_bookmark
  $ quiet hg pull
  $ hg log -r '::remote/master_bookmark' -T '{desc}\n' | sort
  A
  B
  C1
  C2
  PostBatch
  Stack1
  Stack2

Verify one two-commit request is distinguished from two one-commit requests
  $ jq -c 'select(.normal.log_tag == "batched_pushrebase_request_complete" and .int.rebased_changesets == 2) | [.int.changesets_count, .int.batch_size, .int.batch_total_commits]' "$TESTTMP/land_service_scuba.json"
  [2,1,2]

Test batching disabled fallback
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:land_service_batching_enabled": false
  >   }
  > }
  > EOF
  $ cd $TESTTMP/client1
  $ quiet hg pull
  $ quiet hg up remote/master_bookmark
  $ echo "unbatched" > unbatched.txt
  $ hg addremove -q
  $ hg commit -q -m "Unbatched"
  $ quiet hg push --to master_bookmark
  $ quiet hg pull
  $ hg log -r '::remote/master_bookmark' -T '{desc}\n' | sort
  A
  B
  C1
  C2
  PostBatch
  Stack1
  Stack2
  Unbatched
