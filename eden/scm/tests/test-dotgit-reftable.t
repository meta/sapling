#require git no-eden no-windows

  $ . $TESTDIR/git.sh
  $ git init -q -b main --ref-format=reftable repo
  $ cd repo
  $ echo first > a
  $ git add a
  $ git commit -q -m first

Read a repo that stores references in the reftable format.

  $ sl log -T '{desc}\n'
  first

Write a commit that Git can read.

  $ echo second > a
  $ sl commit -q -m second
  $ git log --format=%s
  second
  first
