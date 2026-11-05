# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

A pushrebase over SLAPI of a stack whose bottom commit's change is already on
the destination. The server's pushrebase does not leave such a commit out: it
reports a conflict, or, with merge resolution, lands it with no changes. Either
way the client's warnings about commits the server did not land stay quiet.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setconfig push.edenapi=true
  $ BLOB_TYPE="blob_files" default_setup_drawdag
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Master gets shared.txt:
  $ hg up -q master_bookmark
  $ echo shared > shared.txt
  $ hg ci -Aqm "server: add shared.txt"
  $ hg push -q -r . --to master_bookmark

A stack based before it: X adds the same shared.txt, and Y adds y.txt.
  $ hg up -q .~1
  $ echo shared > shared.txt
  $ hg ci -Aqm X
  $ echo y > y.txt
  $ hg ci -Aqm Y

The push is rejected with a conflict on shared.txt:
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 2 changesets
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("shared.txt"), right: MPath("shared.txt") }]
  [255]

With merge resolution, X lands with no changes and Y on top of it. The server
returns both, so the client warns about neither:
  $ hg push -r . --to master_bookmark --pushvars MERGE_RESOLUTION_OVERRIDE=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master_bookmark to * (glob)
  $ hg log -r 'master_bookmark~2::master_bookmark' -T '{desc}: {files}\n'
  server: add shared.txt: shared.txt
  X: 
  Y: y.txt
