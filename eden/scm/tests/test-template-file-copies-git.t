#require git no-windows no-eden

Test {file_copies} in a git repo

  $ eagerepo
  $ . $RUNTESTDIR/git.sh
  $ setconfig diff.git=true

  $ sl init --git repo1
  $ cd repo1
  $ echo a > a
  $ sl ci -Aqm 'add a'
  $ sl mv a b
  $ sl ci -qm 'mv a -> b'

{file_copies} is not supported in a Git repo: Git does not record copies, so
the list is empty.

  $ sl log -r . -T 'x{file_copies}x\n'
  xx
  $ sl log -r . --copies -T 'x{file_copies_switch}x\n'
  xx

The same holds for the working copy:

  $ sl mv b c
  $ sl log -r 'wdir()' -T 'x{file_copies}x\n'
  xx
