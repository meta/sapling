# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setconfig ui.ignorerevnum=false

setup configuration
  $ export BLOCK_MERGES=1
  $ setconfig push.edenapi=true
  $ setup_common_config
  $ cd $TESTTMP

setup common configuration
  $ cat >> $HGRCPATH <<EOF
  > [ui]
  > ssh="$DUMMYSSH"
  > EOF

setup repo
  $ testtool_drawdag -R repo << EOF
  > C
  > |
  > B
  > |
  > A
  > # bookmark: C master_bookmark
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

start mononoke
  $ start_and_wait_for_mononoke_server
Clone the repo
  $ hg clone -q mono:repo repo2 --noupdate
  $ cd repo2
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > EOF

Try to push merge commit
  $ hg up -q "min(all())"
  $ echo 1 > 1 && hg add 1 && hg ci -m 1
  $ hg up -q "min(all())"
  $ echo 2 > 2 && hg add 2 && hg ci -m 2
  $ hg merge -q -r 26f143b427a3 && hg ci -m "merge 1 and 2"
  $ log -r ":"
  @    merge 1 and 2 [draft;rev=*;540b69c58d33] (glob)
  ├─╮
  │ o  2 [draft;rev=*;d9fe1d08ff73] (glob)
  │ │
  o │  1 [draft;rev=*;26f143b427a3] (glob)
  ├─╯
  │ o  C [public;rev=*;d3b399ca8757] remote/master_bookmark (glob)
  │ │
  │ o  B [public;rev=*;80521a640a0c] (glob)
  ├─╯
  o  A [public;rev=*;20ca2a4749a4] (glob)
  $

With `experimental.edenapi-push-merges` on, the push goes through SLAPI, and the
rejection is a clean abort:
  $ hg push -r . --to master_bookmark --config experimental.edenapi-push-merges=true
  pushing rev 540b69c58d33 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  edenapi: queue 3 commits for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 3 trees for upload
  edenapi: uploaded 3 trees
  edenapi: uploaded 3 changesets
  pushrebasing stack (20ca2a4749a4, 540b69c58d33] (3 commits) to remote bookmark master_bookmark
  abort: Server error: invalid request: Pushrebase blocked because it contains a merge commit.
  If you need this for a specific use case please contact
  the Source Control team at https://fburl.com/27qnuyl2
  [255]

Otherwise the push goes over the wire protocol, where the rejection is
unchanged:
  $ hg push -r . --to master_bookmark
  fallback reason: merge commit is not supported by EdenApi push yet
  pushing rev 540b69c58d33 to destination mono:repo bookmark master_bookmark
  searching for changes
  remote: Command failed
  remote:   Error:
  remote:     Pushrebase blocked because it contains a merge commit.
  remote:     If you need this for a specific use case please contact
  remote:     the Source Control team at https://fburl.com/27qnuyl2
  abort: unexpected EOL, expected netstring digit
  [255]
