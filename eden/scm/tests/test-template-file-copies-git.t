#require git no-windows no-eden

Test {file_copies} in a git repo, whose filelog cannot be iterated

  $ eagerepo
  $ . $RUNTESTDIR/git.sh
  $ setconfig diff.git=true

  $ sl init --git repo1
  $ cd repo1
  $ echo a > a
  $ sl ci -Aqm 'add a'
  $ sl mv a b
  $ sl ci -qm 'mv a -> b'

BUG: {file_copies} and --copies crash with a Python traceback:

  $ sl log -r . -T 'x{file_copies}x\n' 2>&1 | grep -E '^TypeError'
  TypeError: 'gitfilelog' object is not iterable
  $ sl log -r . --copies -T 'x{file_copies_switch}x\n' 2>&1 | grep -E '^TypeError'
  TypeError: 'gitfilelog' object is not iterable

BUG: the working copy crashes too, although the dirstate has the copy:

  $ sl mv b c
  $ sl log -r 'wdir()' -T '{file_copies}\n' 2>&1 | grep -E '^TypeError'
  TypeError: 'gitfilelog' object is not iterable
