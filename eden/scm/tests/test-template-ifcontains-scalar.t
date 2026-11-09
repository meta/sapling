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

An integer haystack is a parse error, like "{rev % ...}" is:

  $ hg log -r $A -T '{ifcontains(rev, rev, "yes", "no")}\n'
  hg: parse error: keyword 'rev' is not iterable
  [255]
  $ hg log -r $A -T "{ifcontains('1', 1, 'yes', 'no')}\n"
  hg: parse error: 1 is not iterable
  [255]
