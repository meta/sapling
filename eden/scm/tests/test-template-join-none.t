Test join() with a list argument that is not a list

  $ enable fbcodereview
  $ newclientrepo
  $ drawdag <<'EOS'
  > A
  > EOS

A list keyword joins as usual; an empty list (reviewers of the working copy,
file_copies) joins to the empty string:

  $ hg log -r $A -T '{join(files, ",")}\n'
  A
  $ hg log -r 'wdir()' -T 'x{join(reviewers, ",")}x\n'
  xx
  $ hg log -r $A -T 'x{join(file_copies, ",")}x\n'
  xx

BUG: a keyword that is not a list crashes with a Python traceback, while
"{rev % ...}" is a clean parse error:

  $ hg log -r $A -T '{rev % "{rev}"}\n'
  hg: parse error: keyword 'rev' is not iterable
  [255]
  $ hg log -r $A -T '{join(rev, ",")}\n' 2>&1 | grep -E '^TypeError'
  TypeError: 'int' object is not iterable
  $ hg log -r $A -T '{join(1, ",")}\n' 2>&1 | grep -E '^TypeError'
  TypeError: 'int' object is not iterable
