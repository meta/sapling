#modern-config-incompatible
#modern-config-incompatible

#require no-eden

#inprocess-hg-incompatible


  $ cat >>$TESTTMP/ccdelay.py <<EOF
  > 
  > import os
  > import time
  > 
  > from sapling import extensions
  > 
  > def extsetup(ui):
  >    cc = extensions.find("commitcloud")
  >    if cc is not None:
  >        extensions.wrapfunction(cc.sync, "_hashrepostate", delayhash)
  > 
  > def delayhash(orig, repo):
  >    ret = orig(repo)
  >    filename = os.environ.get("CCWAITFILE")
  >    if filename:
  >         while os.path.exists(filename):
  >             time.sleep(0.1)
  >    return ret
  > EOF

  $ enable commitcloud amend rebase
  $ configure dummyssh
  $ setconfig commitcloud.hostname=testhost
  $ setconfig remotefilelog.reponame=testrepo
  $ setconfig mutation.record=true mutation.enabled=true
  $ setconfig visibility.enabled=true

  $ newrepo server
  $ setconfig infinitepush.server=yes infinitepush.reponame=testrepo
  $ setconfig infinitepush.indextype=disk infinitepush.storetype=disk
  $ touch base
  $ sl commit -Aqm base
  $ sl bookmark master
  $ cd ..

  $ sl clone ssh://user@dummy/server client1 -q
  $ cd client1
  $ setconfig commitcloud.servicetype=local commitcloud.servicelocation=$TESTTMP
  $ setconfig extensions.extralog="$TESTDIR/extralog.py"
  $ setconfig extralog.events="visibility, commitcloud_sync"
  $ setconfig extensions.ccdelay="$TESTTMP/ccdelay.py"
  $ sl cloud join
  commitcloud: this repository is now connected to the 'user/test/default' workspace for the 'testrepo' repo
  commitcloud: synchronizing 'testrepo' with 'user/test/default'
  visibility: read 0 heads: 
  commitcloud: nothing to upload
  commitcloud_sync: synced to workspace user/test/default version 1: 0 heads (0 omitted), 0 bookmarks (0 omitted), 0 remote bookmarks (0 omitted)
  commitcloud: commits synchronized
  finished in 0.00 sec
  $ cd ..

  $ sl clone ssh://user@dummy/server client2 -q
  $ cd client2
  $ setconfig commitcloud.servicetype=local commitcloud.servicelocation=$TESTTMP
  $ setconfig extensions.extralog="$TESTDIR/extralog.py"
  $ setconfig extralog.events="visibility, commitcloud_sync"
  $ setconfig extensions.ccdelay="$TESTTMP/ccdelay.py"
  $ sl cloud join
  commitcloud: this repository is now connected to the 'user/test/default' workspace for the 'testrepo' repo
  commitcloud: synchronizing 'testrepo' with 'user/test/default'
  visibility: read 0 heads: 
  commitcloud: nothing to upload
  visibility: read 0 heads: 
  commitcloud_sync: synced to workspace user/test/default version 1: 0 heads (0 omitted), 0 bookmarks (0 omitted), 0 remote bookmarks (0 omitted)
  commitcloud: commits synchronized
  finished in 0.00 sec
  $ cd ..

  $ cd client1
  $ touch 1
  $ sl commit -Aqm commit1
  visibility: read 0 heads: 
  visibility: removed 0 heads []; added 1 heads [79089e97b9e7]
  visibility: wrote 1 heads: 79089e97b9e7
  $ sl cloud sync
  commitcloud: synchronizing 'testrepo' with 'user/test/default'
  visibility: read 1 heads: 79089e97b9e7
  commitcloud: head '79089e97b9e7' hasn't been uploaded yet
  edenapi: queue 1 commit for upload
  edenapi: queue 0 files for upload
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  commitcloud_sync: synced to workspace user/test/default version 2: 1 heads (0 omitted), 0 bookmarks (0 omitted), 0 remote bookmarks (0 omitted)
  commitcloud: commits synchronized
  finished in 0.00 sec

  $ cd ../client2

Start a background sync to pull in the changes from the other repo.

  $ touch $TESTTMP/ccdelay1
  $ CCWAITFILE=$TESTTMP/ccdelay1 sl cloud sync --best-effort > $TESTTMP/bgsync.out 2>&1 &

While that is getting started, create a new commit locally.

  $ sleep 1
  $ touch 2
  $ sl commit -Aqm commit2
  visibility: read 0 heads: 
  visibility: removed 0 heads []; added 1 heads [1292cc1f1c17]
  visibility: wrote 1 heads: 1292cc1f1c17
  $ sl up -q 'desc(base)'
  visibility: read 1 heads: 1292cc1f1c17
  $ tglogp
  visibility: read 1 heads: 1292cc1f1c17
  o  1292cc1f1c17 draft 'commit2'
  │
  @  df4f53cec30a public 'base'

Let the background sync we started earlier continue, and start a concurrent cloud sync.

  $ rm $TESTTMP/ccdelay1
  $ sl cloud sync --best-effort
  commitcloud: synchronizing 'testrepo' with 'user/test/default'
  visibility: read 1 heads: 1292cc1f1c17
  commitcloud: head '1292cc1f1c17' hasn't been uploaded yet
  edenapi: queue 1 commit for upload
  edenapi: queue 0 files for upload
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  visibility: removed 0 heads []; added 1 heads [79089e97b9e7] (?)
  visibility: wrote 2 heads: 79089e97b9e7, 1292cc1f1c17 (?)
  pulling 79089e97b9e7 from ssh://user@dummy/server
  searching for changes
  visibility: removed 0 heads []; added 1 heads [79089e97b9e7]
  visibility: removed 1 heads [79089e97b9e7]; added 0 heads []
  visibility: wrote 1 heads: 1292cc1f1c17
  visibility: read 1 heads: 1292cc1f1c17
  visibility: removed 0 heads []; added 1 heads [79089e97b9e7]
  commitcloud_sync: synced to workspace user/test/default version 2: 1 heads (0 omitted), 0 bookmarks (0 omitted), 0 remote bookmarks (0 omitted)
  visibility: wrote 2 heads: 79089e97b9e7, 1292cc1f1c17
  commitcloud_sync: synced to workspace user/test/default version 3: 2 heads (0 omitted), 0 bookmarks (0 omitted), 0 remote bookmarks (0 omitted)
  commitcloud: commits synchronized
  finished in 0.00 sec

  $ tglogp
  visibility: read 2 heads: 79089e97b9e7, 1292cc1f1c17
  o  79089e97b9e7 draft 'commit1'
  │
  │ o  1292cc1f1c17 draft 'commit2'
  ├─╯
  @  df4f53cec30a public 'base'

Wait for the background backup to finish and check its output.

  $ sl debugwaitbackup
  $ cat $TESTTMP/bgsync.out
  commitcloud: synchronizing 'testrepo' with 'user/test/default'
  visibility: read 0 heads: 
  commitcloud: nothing to upload
  abort: commitcloud: failed to synchronize commits: 'repo changed while backing up'
  (please retry 'sl cloud sync')
  (please contact the Source Control Team if this error persists)
