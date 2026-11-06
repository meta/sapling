#inprocess-hg-incompatible
#require eden

Run Sapling out of process so Windows releases repository directory handles
before "eden rm".

Basic test demonstrating that ".sl" dir is now a symlink in eden mounts.

  $ newclientrepo repo
  $ ls -l .sl
  lrwx* .sl -> */eden/clients/repo/sl-repo-dir (glob)
  $ touch foo
  $ sl commit -Aqm foo
  $ sl push -q --to master --create
  $ sl log -G
  @  commit:      1f7b0de80e11
     bookmark:    remote/master
     hoistedname: master
     user:        test
     date:        Thu Jan 01 00:00:00 1970 +0000
     summary:     foo

  $ cd
  $ eden rm repo 2>/dev/null
  Removing $TESTTMP/repo...
  Stopping aux processes for $TESTTMP/repo...
  Unmounting `$TESTTMP/repo`. Please be patient: this can take up to 1 minute!
  Cleaning up mount $TESTTMP/repo...
  Deleting mount $TESTTMP/repo...
  Success
