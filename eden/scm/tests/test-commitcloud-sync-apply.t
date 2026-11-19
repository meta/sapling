#require no-eden

  $ configure modernclient
  $ setconfig infinitepushbackup.autobackup=false

An extension that makes merging the workspace's changes fail.  Once a sync has
fetched them, it can also hold the repo lock in another process, or pull master
in another process.

  $ cat > $TESTTMP/applytest.py <<'PY'
  > import subprocess
  > import sys
  > from sapling import error, extensions, registrar, util
  > cmdtable = {}
  > command = registrar.command(cmdtable)
  > holder = None
  > @command('debugholdlock', [], '')
  > def holdlock(ui, repo):
  >     with repo.lock():
  >         print('locked', flush=True)
  >         sys.stdin.readline()
  > def extsetup(ui):
  >     sync = extensions.find('commitcloud').sync
  >     extensions.wrapfunction(sync, '_mergebookmarks', mergebookmarks)
  >     extensions.wrapfunction(sync, '_fetchcloudchanges', fetchcloudchanges)
  >     extensions.wrapfunction(sync, '_sync', dosync)
  > def mergebookmarks(orig, repo, *args, **kwargs):
  >     if repo.ui.configbool('applytest', 'add-and-fail'):
  >         child = subprocess.run(util.hgcmd() + ['--cwd', repo.root, 'add', 'added-during-sync'], capture_output=True, text=True, timeout=10)
  >         assert child.returncode == 0, child.stdout + child.stderr
  >         raise error.Abort('injected failure')
  >     return orig(repo, *args, **kwargs)
  > def fetchcloudchanges(orig, repo, *args, **kwargs):
  >     fetched = orig(repo, *args, **kwargs)
  >     if repo.ui.configbool('applytest', 'pull-master'):
  >         child = subprocess.run(util.hgcmd() + ['--cwd', repo.root, 'pull', '-q', '-B', 'master'], capture_output=True, text=True, timeout=15)
  >         assert child.returncode == 0, child.stdout + child.stderr
  >     if repo.ui.configbool('applytest', 'hold-lock'):
  >         def hold(_tr):
  >             global holder
  >             holder = subprocess.Popen(util.hgcmd() + ['--cwd', repo.root, 'debugholdlock'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
  >             assert holder.stdout.readline().strip() == 'locked'
  >         repo.currenttransaction().addpostclose('zz-applytest-hold-lock', hold)
  >     return fetched
  > def dosync(orig, *args, **kwargs):
  >     global holder
  >     try:
  >         return orig(*args, **kwargs)
  >     finally:
  >         if holder is not None:
  >             output, _ = holder.communicate('release\n', timeout=10)
  >             assert holder.returncode == 0, output
  >             holder = None
  > PY
  $ setconfig extensions.applytest=$TESTTMP/applytest.py

  $ newclientrepo c1 server master <<'EOS'
  > M  # bookmark master = M
  > EOS
  $ setconfig remotefilelog.reponame=server
  $ sl cloud join -q -w abort
  $ newclientrepo c2 server master
  $ setconfig remotefilelog.reponame=server
  $ sl cloud join -q -w abort

Sync merges the workspace's changes holding the repo lock but not the working
copy lock.  If the merge fails, it must not undo a working copy change made
meanwhile.

  $ cd $TESTTMP/c1
  $ drawdag <<'EOS'
  > A
  > |
  > M
  > EOS
  $ sl cloud sync -q
  $ cd $TESTTMP/c2
  $ touch added-during-sync
  $ sl cloud sync -q --config applytest.add-and-fail=true
  abort: injected failure
  [255]
  $ sl status
  A added-during-sync

If a best-effort sync finds the repo lock held when it is ready to merge, it
gives up.  The commits it pulled stay hidden until a later sync merges them
together with the sync state that records them, so hiding them afterwards
works.

  $ newclientrepo c3 server master
  $ setconfig remotefilelog.reponame=server
  $ sl cloud join -q -w lock
  $ newclientrepo c4 server master
  $ setconfig remotefilelog.reponame=server
  $ sl cloud join -q -w lock
  $ cd $TESTTMP/c3
  $ drawdag <<'EOS'
  > H
  > |
  > M
  > EOS
  $ sl cloud sync -q
  $ cd $TESTTMP/c4
  $ sl cloud sync --best-effort -q --config applytest.hold-lock=true > $TESTTMP/failed-sync.out 2>&1 || test $? -eq 255
  $ sl log -r 'draft()' -T '{desc}\n'
  $ sl cloud sync -q
  $ sl log -r 'draft()' -T '{desc}\n'
  H
  $ sl hide -q -r 'desc(H)'
  $ sl cloud sync -q
  $ cd $TESTTMP/c3
  $ sl cloud sync -q
  $ sl log -r 'draft()' -T '{desc}\n'

A pull that runs while a sync is fetching can bring in the commit a cloud
bookmark points to.  The merge must use what the repo has when it runs.

  $ newclientrepo c5 server2 master <<'EOS'
  > M  # bookmark master = M
  > EOS
  $ setconfig remotefilelog.reponame=server2 commitcloud.remotebookmarkssync=false commitcloud.max_sync_age=14
  $ sl bookmark b -r $M
  $ sl cloud join -q -w bookmark
  $ newclientrepo c6 server2 master
  $ setconfig remotefilelog.reponame=server2 commitcloud.remotebookmarkssync=false commitcloud.max_sync_age=14
  $ sl bookmark b -r $M
  $ sl cloud join -q -w bookmark

Client 5 moves b to H, a new public commit that client 6 doesn't have.

  $ cd $TESTTMP/server2
  $ drawdag <<EOS
  > H  # bookmark master = H
  > |
  > $M
  > EOS
  $ cd $TESTTMP/c5
  $ sl pull -q -B master
  $ sl bookmark -f b -r $H
  $ sl cloud sync -q
  $ cd $TESTTMP/c6
  $ sl cloud sync -q --config applytest.pull-master=true
  $ sl log -r 'bookmark(b)' -T '{desc}\n'
  H

A remote bookmark fetched separately because it's expensive stays hidden until
the merge too.

  $ newclientrepo c7 server3 master <<'EOS'
  > M  # bookmark master = M
  > EOS
  $ setconfig remotefilelog.reponame=server3 commitcloud.expensive_bookmarks=feature
  $ sl cloud join -q -w expensive
  $ newclientrepo c8 server3 master
  $ setconfig remotefilelog.reponame=server3 commitcloud.expensive_bookmarks=feature
  $ sl cloud join -q -w expensive
  $ cd $TESTTMP/server3
  $ drawdag <<EOS
  > B  # bookmark feature = B
  > |
  > $M
  > EOS
  $ cd $TESTTMP/c7
  $ sl pull -q -B feature
  $ sl cloud sync -q
  $ cd $TESTTMP/c8
  $ sl cloud sync --best-effort -q --config applytest.hold-lock=true > $TESTTMP/failed-sync.out 2>&1
  [255]
  $ sl log -r 'draft()' -T '{desc}\n'
  $ sl cloud sync -q
  $ sl log -r 'draft()' -T '{desc}\n'

A workspace can list a commit and its parent as separate heads.  A sync that
pulls them separately must keep both hidden until the merge.

  $ newclientrepo c9 server4 master <<'EOS'
  > M  # bookmark master = M
  > EOS
  $ setconfig remotefilelog.reponame=server4
  $ sl cloud join -q -w heads
  $ newclientrepo c11 server4 master
  $ setconfig remotefilelog.reponame=server4 commitcloud.pullsizelimit=1
  $ sl cloud join -q -w heads
  $ cd $TESTTMP/c9
  $ drawdag <<'EOS'
  > P
  > |
  > M
  > EOS
  $ sl cloud sync -q
  $ newclientrepo c10 server4 master
  $ setconfig remotefilelog.reponame=server4 commitcloud.max_sync_age=14
  $ sl cloud join -q -w heads
  $ cd $TESTTMP/c9
  $ drawdag <<EOS
  > C
  > |
  > $P
  > EOS
  $ sl cloud sync -q

The test service reports dates for the heads listed in its node data.  C's date
is old.

  $ sl log -r $C -Tjson > $TESTTMP/nodedatausertestheads

Client 10 leaves out C, which is old, but keeps P, which C is based on.  It
sends both to the workspace as heads.

  $ cd $TESTTMP/c10
  $ sl cloud sync -q
  $ sl log -r 'draft()' -T '{desc}\n'
  P

Client 11 syncs everything, one head per pull, and can't merge.  Pulling C
brings in P too, so pulling P finds it already there.

  $ cd $TESTTMP/c11
  $ sl cloud sync --full --best-effort -q --config applytest.hold-lock=true > $TESTTMP/failed-sync.out 2>&1
  [255]
  $ sl log -r 'draft()' -T '{desc}\n'
  $ sl cloud sync --full -q
  $ sl log -r 'sort(draft(), desc)' -T '{desc}\n'
  C
  P

A pull that runs while a sync is fetching keeps the remote bookmark it moved.

  $ newclientrepo c12 server5 master <<'EOS'
  > M  # bookmark master = M
  > EOS
  $ setconfig remotefilelog.reponame=server5
  $ sl cloud join -q -w remote
  $ newclientrepo c13 server5 master
  $ setconfig remotefilelog.reponame=server5
  $ sl cloud join -q -w remote
  $ cd $TESTTMP/server5
  $ drawdag <<EOS
  > N  # bookmark master = N
  > |
  > $M
  > EOS

Client 12 adds a commit to the workspace.  Client 13 pulls master to N while
its sync is fetching the commit.

  $ cd $TESTTMP/c12
  $ drawdag <<EOS
  > X
  > |
  > $M
  > EOS
  $ sl cloud sync -q
  $ cd $TESTTMP/c13
  $ sl cloud sync -q --config applytest.pull-master=true
  $ sl log -r remote/master -T '{desc}\n'
  N

If the workspace and the pull both move it, the sync keeps the newer commit.

  $ cd $TESTTMP/c12
  $ sl cloud sync -q
  $ cd $TESTTMP/server5
  $ drawdag <<EOS
  > N1  # bookmark master = N1
  > |
  > $N
  > EOS
  $ cd $TESTTMP/c12
  $ sl pull -q -B master
  $ sl cloud sync -q
  $ cd $TESTTMP/server5
  $ drawdag <<EOS
  > N2  # bookmark master = N2
  > |
  > $N1
  > EOS
  $ cd $TESTTMP/c13
  $ sl cloud sync -q --config applytest.pull-master=true
  $ sl log -r remote/master -T '{desc}\n'
  N2
