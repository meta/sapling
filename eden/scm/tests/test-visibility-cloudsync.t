#modern-config-incompatible

#require no-eden


  $ configure dummyssh
  $ enable amend directaccess commitcloud rebase undo
  $ setconfig infinitepush.branchpattern="re:scratch/.*"
  $ setconfig commitcloud.hostname=testhost
  $ setconfig visibility.enabled=true
  $ setconfig experimental.evolution=obsolete
  $ setconfig mutation.record=true mutation.enabled=true mutation.user=test
  $ setconfig remotefilelog.reponame=server
  $ setconfig hint.ack='*'

  $ newrepo server
  $ setconfig infinitepush.server=yes infinitepush.indextype=disk infinitepush.storetype=disk infinitepush.reponame=testrepo
  $ echo base > base
  $ sl commit -Aqm base
  $ sl bookmark master

Create a client with some initial commits and sync them to the cloud workspace.

  $ cd $TESTTMP
  $ sl clone ssh://user@dummy/server client1 -q
  $ cd client1
  $ setconfig commitcloud.servicetype=local commitcloud.servicelocation=$TESTTMP
  $ sl cloud join
  commitcloud: this repository is now connected to the 'user/test/default' workspace for the 'server' repo
  commitcloud: synchronizing 'server' with 'user/test/default'
  commitcloud: nothing to upload
  commitcloud: commits synchronized
  finished in * sec (glob)
  $ drawdag << EOS
  > B D    # amend: A -> C -> E
  > | |    # rebase: B -> D
  > A C E
  >  \|/
  >   Z
  >   |
  > d20a80d4def3
  > EOS
  $ tglogm
  o  c70a9bd6bfd1 'E'
  │
  │ o  6ba5de8abe43 'D'
  │ │
  │ x  2d0f0af04f18 'C'  (Rewritten using amend into c70a9bd6bfd1)
  ├─╯
  o  dae3b312bb78 'Z'
  │
  @  d20a80d4def3 'base'
  
  $ sl cloud sync
  commitcloud: synchronizing 'server' with 'user/test/default'
  commitcloud: head '6ba5de8abe43' hasn't been uploaded yet
  commitcloud: head 'c70a9bd6bfd1' hasn't been uploaded yet
  edenapi: queue 4 commits for upload
  edenapi: queue 4 files for upload
  edenapi: uploaded 4 files
  edenapi: queue 4 trees for upload
  edenapi: uploaded 4 trees
  edenapi: uploaded 4 changesets
  commitcloud: commits synchronized
  finished in * sec (glob)

Create another client and use it to modify the commits and create some new ones.

  $ cd $TESTTMP
  $ sl clone ssh://user@dummy/server client2 -q
  $ cd client2
  $ setconfig commitcloud.servicetype=local commitcloud.servicelocation=$TESTTMP
  $ sl cloud join
  commitcloud: this repository is now connected to the 'user/test/default' workspace for the 'server' repo
  commitcloud: synchronizing 'server' with 'user/test/default'
  commitcloud: nothing to upload
  pulling 6ba5de8abe43 c70a9bd6bfd1 from ssh://user@dummy/server
  searching for changes
  commitcloud: commits synchronized
  finished in * sec (glob)
  $ tglogm
  o  c70a9bd6bfd1 'E'
  │
  │ o  6ba5de8abe43 'D'
  │ │
  │ x  2d0f0af04f18 'C'  (Rewritten using amend into c70a9bd6bfd1)
  ├─╯
  o  dae3b312bb78 'Z'
  │
  @  d20a80d4def3 'base'
  

  $ sl rebase -r $D -d $E
  rebasing 6ba5de8abe43 "D"
  $ sl up -q $Z
  $ echo X > X
  $ sl commit -Aqm X
  $ tglogm
  @  dd114d9b2f9e 'X'
  │
  │ o  d8fc5ae9b7ef 'D'
  │ │
  │ o  c70a9bd6bfd1 'E'
  ├─╯
  o  dae3b312bb78 'Z'
  │
  o  d20a80d4def3 'base'
  
  $ sl cloud sync
  commitcloud: synchronizing 'server' with 'user/test/default'
  commitcloud: head 'd8fc5ae9b7ef' hasn't been uploaded yet
  commitcloud: head 'dd114d9b2f9e' hasn't been uploaded yet
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  commitcloud: commits synchronized
  finished in * sec (glob)

Before syncing, create a new commit in the original client

  $ cd $TESTTMP/client1
  $ sl up -q $E
  $ echo F > F
  $ sl commit -Aqm F

Also introduce some divergence by rebasing the same commit

  $ sl rebase -r $D -d $Z
  rebasing 6ba5de8abe43 "D"

Now cloud sync.  The sets of commits should be merged.

  $ tglogm
  o  6caded0e9807 'D'
  │
  │ @  ba83c5428cb2 'F'
  │ │
  │ o  c70a9bd6bfd1 'E'
  ├─╯
  o  dae3b312bb78 'Z'
  │
  o  d20a80d4def3 'base'
  
  $ sl cloud sync
  commitcloud: synchronizing 'server' with 'user/test/default'
  commitcloud: head 'ba83c5428cb2' hasn't been uploaded yet
  commitcloud: head '6caded0e9807' hasn't been uploaded yet
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  pulling d8fc5ae9b7ef dd114d9b2f9e from ssh://user@dummy/server
  searching for changes
  commitcloud: commits synchronized
  finished in * sec (glob)
  $ tglogm
  o  dd114d9b2f9e 'X'
  │
  │ o  d8fc5ae9b7ef 'D'
  │ │
  │ │ o  6caded0e9807 'D'
  ├───╯
  │ │ @  ba83c5428cb2 'F'
  │ ├─╯
  │ o  c70a9bd6bfd1 'E'
  ├─╯
  o  dae3b312bb78 'Z'
  │
  o  d20a80d4def3 'base'
  

Cloud sync back to the other client, it should get the same smartlog (apart from ordering).

  $ cd $TESTTMP/client2
  $ sl cloud sync
  commitcloud: synchronizing 'server' with 'user/test/default'
  commitcloud: nothing to upload
  pulling ba83c5428cb2 6caded0e9807 from ssh://user@dummy/server
  searching for changes
  commitcloud: commits synchronized
  finished in * sec (glob)
  $ tglogm
  o  6caded0e9807 'D'
  │
  │ o  ba83c5428cb2 'F'
  │ │
  │ │ @  dd114d9b2f9e 'X'
  ├───╯
  │ │ o  d8fc5ae9b7ef 'D'
  │ ├─╯
  │ o  c70a9bd6bfd1 'E'
  ├─╯
  o  dae3b312bb78 'Z'
  │
  o  d20a80d4def3 'base'
It should also have mutations made on both sides visible.

  $ tglogm -r 'predecessors(all())'
  o  6caded0e9807 'D'
  │
  │ o  ba83c5428cb2 'F'
  │ │
  │ │ @  dd114d9b2f9e 'X'
  ├───╯
  │ │ o  d8fc5ae9b7ef 'D'
  │ ├─╯
  │ o  c70a9bd6bfd1 'E'
  ├─╯
  │ v  6ba5de8abe43 'D'  (Rewritten using rebase into 6caded0e9807) (Rewritten using rebase into d8fc5ae9b7ef)
  │ │
  │ x  2d0f0af04f18 'C'  (Rewritten using amend into c70a9bd6bfd1)
  ├─╯
  o  dae3b312bb78 'Z'
  │
  o  d20a80d4def3 'base'
