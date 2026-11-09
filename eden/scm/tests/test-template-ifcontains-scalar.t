Test ifcontains() with a haystack that is not a container

  $ enable fbcodereview
  $ newclientrepo
  $ drawdag <<'EOS'
  > A
  > EOS

A container haystack works as usual:

  $ hg log -r $A -T '{ifcontains(rev, revset("all()"), "yes", "no")}\n'
  yes
  $ hg log -r $A -T '{ifcontains("b", "abc", "yes", "no")}\n'
  yes

An empty list (reviewers of the working copy) contains nothing:

  $ hg log -r 'wdir()' -T '{ifcontains("x", reviewers, "yes", "no")}\n'
  no

BUG: an integer haystack crashes with a Python traceback:

  $ hg log -r $A -T '{ifcontains(rev, rev, "yes", "no")}\n' 2>&1 | grep -E '^TypeError'
  TypeError: argument of type 'int' is not iterable
  $ hg log -r $A -T "{ifcontains('1', 1, 'yes', 'no')}\n" 2>&1 | grep -E '^TypeError'
  TypeError: argument of type 'int' is not iterable
