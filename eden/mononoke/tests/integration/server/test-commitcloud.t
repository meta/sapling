# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setconfig ui.ignorerevnum=false

setup configuration
  $ export READ_ONLY_REPO=1
  $ INFINITEPUSH_ALLOW_WRITES=true \
  >   setup_common_config
  $ cd $TESTTMP

setup common configuration for these tests
mononoke + local commit cloud backend
  $ cat >> $HGRCPATH <<EOF
  > [extensions]
  > amend =
  > commitcloud =
  > rebase =
  > [commitcloud]
  > servicetype = local
  > servicelocation = $TESTTMP
  > updateonmove = true
  > EOF

setup repo

  $ testtool_drawdag --print-hg-hashes -R repo --derive-all --no-default-files <<EOF
  > A
  > # modify: A "a" "file_content"
  > # bookmark: A master_bookmark
  > # message: A "base_commit"
  > EOF
  A=f4292546bbf22a29348935427ccd5b8ea2f3aa33

start mononoke

  $ start_and_wait_for_mononoke_server

setup client1 and client2
  $ hg clone -q mono:repo client1 --noupdate
  $ hg clone -q mono:repo client2 --noupdate
  $ cd client1
  $ hg cloud join
  commitcloud: this repository is now connected to the 'user/test/default' workspace for the 'repo' repo
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  commitcloud: commits synchronized
  finished in * (glob)
  $ hg up master_bookmark -q
  $ cd ../client2
  $ hg cloud join
  commitcloud: this repository is now connected to the 'user/test/default' workspace for the 'repo' repo
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  commitcloud: commits synchronized
  finished in * (glob)
  $ hg up master_bookmark -q


Make commits in the first client, and sync it
  $ cd ../client1
  $ mkcommit "commit1"
  $ mkcommit "commit2"
  $ mkcommit "commit3"
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: head '37ed84dd5e70' hasn't been uploaded yet
  edenapi: queue 3 commits for upload
  edenapi: queue 3 files for upload
  edenapi: uploaded 3 files
  edenapi: queue 3 trees for upload
  edenapi: uploaded 3 trees
  edenapi: uploaded 3 changesets
  commitcloud: commits synchronized
  finished in * (glob)

  $ tglogp
  @  37ed84dd5e70 draft 'commit3'
  │
  o  4867355284e4 draft 'commit2'
  │
  o  89bab2e12da2 draft 'commit1'
  │
  o  f4292546bbf2 public 'base_commit'
  
Sync from the second client - the commits should appear
  $ cd ../client2
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  pulling 37ed84dd5e70 from mono:repo
  searching for changes
  commitcloud: commits synchronized
  finished in * (glob)

  $ tglogp
  o  37ed84dd5e70 draft 'commit3'
  │
  o  4867355284e4 draft 'commit2'
  │
  o  89bab2e12da2 draft 'commit1'
  │
  @  f4292546bbf2 public 'base_commit'
  

Make commits from the second client and sync it
  $ mkcommit "commit4"
  $ mkcommit "commit5"
  $ mkcommit "commit6"
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: head '732edd29dff2' hasn't been uploaded yet
  edenapi: queue 3 commits for upload
  edenapi: queue 3 files for upload
  edenapi: uploaded 3 files
  edenapi: queue 3 trees for upload
  edenapi: uploaded 3 trees
  edenapi: uploaded 3 changesets
  commitcloud: commits synchronized
  finished in * (glob)


On the first client, make a bookmark, then sync - the bookmark and the new commits should be synced
  $ cd ../client1
  $ hg bookmark -r "min(all())" bookmark1
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  pulling 732edd29dff2 from mono:repo
  searching for changes
  commitcloud: commits synchronized
  finished in * (glob)

  $ tglogp
  o  732edd29dff2 draft 'commit6'
  │
  o  1006c3d2da01 draft 'commit5'
  │
  o  48ad5bad9631 draft 'commit4'
  │
  │ @  37ed84dd5e70 draft 'commit3'
  │ │
  │ o  4867355284e4 draft 'commit2'
  │ │
  │ o  89bab2e12da2 draft 'commit1'
  ├─╯
  o  f4292546bbf2 public 'base_commit' bookmark1
  
 
On the first client rebase the stack
  $ hg rebase -s 48ad5bad9631 -d 37ed84dd5e70
  rebasing 48ad5bad9631 "commit4"
  rebasing 1006c3d2da01 "commit5"
  rebasing 732edd29dff2 "commit6"
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: head '1ff094a3d8b7' hasn't been uploaded yet
  edenapi: queue 3 commits for upload
  edenapi: queue 0 files for upload
  edenapi: queue 3 trees for upload
  edenapi: uploaded 3 trees
  edenapi: uploaded 3 changesets
  commitcloud: commits synchronized
  finished in * (glob)


On the second client sync it
  $ cd ../client2
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  pulling 1ff094a3d8b7 from mono:repo
  searching for changes
  commitcloud: commits synchronized
  finished in * (glob)
  commitcloud: current revision 732edd29dff2 has been moved remotely to 1ff094a3d8b7
  updating to 1ff094a3d8b7
  3 files updated, 0 files merged, 0 files removed, 0 files unresolved

  $ tglogp
  @  1ff094a3d8b7 draft 'commit6'
  │
  o  2449d7845608 draft 'commit5'
  │
  o  40afcb5da906 draft 'commit4'
  │
  o  37ed84dd5e70 draft 'commit3'
  │
  o  4867355284e4 draft 'commit2'
  │
  o  89bab2e12da2 draft 'commit1'
  │
  o  f4292546bbf2 public 'base_commit' bookmark1
  

On the second client hide all draft commits
  $ hg hide -r 'draft()'
  hiding commit 89bab2e12da2 "commit1"
  hiding commit 4867355284e4 "commit2"
  hiding commit 37ed84dd5e70 "commit3"
  hiding commit 40afcb5da906 "commit4"
  hiding commit 2449d7845608 "commit5"
  hiding commit 1ff094a3d8b7 "commit6"
  0 files updated, 0 files merged, 6 files removed, 0 files unresolved
  working directory now at f4292546bbf2
  6 changesets hidden
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  commitcloud: commits synchronized
  finished in * (glob)
  $ hg up master_bookmark -q

  $ tglogp
  @  f4292546bbf2 public 'base_commit' bookmark1
  

On the first client check that all commits were hidden
  $ cd ../client1
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  commitcloud: commits synchronized
  finished in * (glob)
  $ hg up master_bookmark -q

  $ tglogp
  @  f4292546bbf2 public 'base_commit' bookmark1
  
 
On the first client make 2 stacks
  $ mkcommit 'stack 1 first'
  $ mkcommit 'stack 1 second'
  $ hg up -q -r 0
  $ mkcommit 'stack 2 first'
  $ mkcommit 'stack 2 second'

  $ tglogp
  @  58da2fa41628 draft 'stack 2 second'
  │
  o  9ac275085c59 draft 'stack 2 first'
  │
  │ o  53e6d921f416 draft 'stack 1 second'
  │ │
  │ o  f10e962f3f60 draft 'stack 1 first'
  ├─╯
  o  f4292546bbf2 public 'base_commit' bookmark1
  
Make one of the commits public when it shouldn't be.
  $ hg debugmakepublic f10e962f3f60
  $ hg cloud sync 2>&1 | grep fail
  commitcloud: failed to synchronize 53e6d921f416

  $ hg debugmakepublic --delete f10e962f3f60
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: head '53e6d921f416' hasn't been uploaded yet
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 2 changesets
  commitcloud: commits synchronized
  finished in 0.00 sec

Commit still becomes available in the other repo
  $ cd ../client2
  $ hg cloud sync
  commitcloud: synchronizing 'repo' with 'user/test/default'
  commitcloud: nothing to upload
  pulling 58da2fa41628 53e6d921f416 from mono:repo
  searching for changes
  commitcloud: commits synchronized
  finished in * (glob)

# Mononoke order is not stable, so the stacks print stacks separately
  $ tglogpnr -r "::53e6d921f416 - ::master_bookmark"
  o  53e6d921f416 draft 'stack 1 second'
  │
  o  f10e962f3f60 draft 'stack 1 first'
  │
  ~
  $ tglogpnr -r "::58da2fa41628 - ::master_bookmark"
  o  58da2fa41628 draft 'stack 2 second'
  │
  o  9ac275085c59 draft 'stack 2 first'
  │
  ~

Fix up that public commit, set it back to draft
  $ cd ../client1
  $ hg debugmakepublic -d f10e962f3f60

Clean up
  $ hg hide -r 'draft()' -q
  $ hg cloud sync -q
  $ cd ../client2
  $ hg cloud sync -q

  $ tglogp
  @  f4292546bbf2 public 'base_commit' bookmark1
  
