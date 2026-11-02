
#require no-eden


  $ eagerepo
  $ enable mergedriver
  $ setconfig commands.update.check=none devel.collapse-traceback=true

basic merge driver: just lists out files and contents, doesn't resolve any files

  $ cat > mergedriver-list.py << EOF
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >    for f in mergestate:
  >        repo.ui.status('%s %s\n' % (mergestate[f].upper(), f))
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     pass
  > EOF

  $ sl init repo1
  $ cd repo1
  $ echo afoo > foo.txt
  $ echo abar > bar.txt
  $ sl add foo.txt bar.txt
  $ sl commit -ma
  $ echo bfoo >> foo.txt
  $ echo bbar >> bar.txt
  $ sl commit -mb
  $ sl up 'desc(a)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo cfoo >> foo.txt
  $ echo cbar >> bar.txt
  $ sl commit -mc
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-list.py
  > EOF
  $ sl merge 'desc(b)'
  U bar.txt
  U foo.txt
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  merging foo.txt
  warning: 1 conflicts while merging foo.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl debugmergestate | grep 'merge driver:'
  merge driver: python:$TESTTMP/mergedriver-list.py (state "s")
  $ sl resolve --list
  U bar.txt
  U foo.txt
  $ sl resolve --all --tool internal:local
  (no more unresolved files)
  $ sl commit -m merge

merge driver that always takes other versions
(rc = 0, unresolved = n, driver-resolved = n)

  $ cat > ../mergedriver-other.py << EOF
  > import bindings
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     overrides = {('ui', 'forcemerge'): ':other'}
  >     with ui.configoverride(overrides, 'mergedriver'):
  >         ui.setconfig('ui', 'forcemerge', ':other', 'mergedriver')
  >         mergestate.preresolve('foo.txt', wctx)
  >         mergestate.resolve('foo.txt', wctx)
  >         mergestate.preresolve('bar.txt', wctx)
  >         mergestate.resolve('bar.txt', wctx)
  >         mergestate.commit()
  >     bindings.commands.run(["hg", "debugmergestate"])
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     pass
  > EOF

  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-other.py
  > EOF
  $ sl up --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl merge 'desc(b)'
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-other.py (state "s")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "r", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "r", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  0 files updated, 2 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)

mark a file driver-resolved, and leave others unresolved
(r = False, unresolved = y, driver-resolved = y)

  $ cat > ../mergedriver-auto1.py << EOF
  > from sapling import util
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     mergestate.mark('foo.txt', 'd')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     util.copyfile(repo.wjoin('bar.txt'), repo.wjoin('foo.txt'))
  >     mergestate.mark('foo.txt', 'r')
  > EOF
  $ sl up --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver=python:$TESTTMP/mergedriver-auto1.py
  > EOF
  $ sl merge 'desc(b)'
  * preprocess called
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl summary
  parent: ede3d67b8d0f 
   c
  parent: e0cfe070a2bb 
   b
  commit: 2 modified, 2 unknown, 1 unresolved (merge)
  phases: 4 draft
  $ sl resolve --list
  U bar.txt
  D foo.txt
  $ sl debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-auto1.py (state "m")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "u", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "D", state "d", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl resolve bar.txt --tool internal:local
  (no more unresolved files -- run "sl resolve --all" to conclude)
  $ sl resolve --list
  R bar.txt
  D foo.txt
  $ sl debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-auto1.py (state "m")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "r", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "D", state "d", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92

  $ sl resolve --all
  * conclude called
  (no more unresolved files)
  $ sl resolve --list
  R bar.txt
  R foo.txt
  $ sl commit -m 'merged'
  $ cat foo.txt
  abar
  cbar

mark a file driver-resolved, and leave others unresolved (but skip merge driver)
(r = False, unresolved = y, driver-resolved = y)
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl merge 'desc(b)'
  * preprocess called
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl resolve --list
  U bar.txt
  D foo.txt
  $ sl debugmergestate | grep 'merge driver:'
  merge driver: python:$TESTTMP/mergedriver-auto1.py (state "m")
  $ sl resolve --all --skip
  warning: skipping merge driver (you MUST regenerate artifacts afterwards)
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  [1]
  $ sl resolve --list
  U bar.txt
  U foo.txt
  $ sl debugmergestate | grep 'merge driver:'
  [1]
  $ sl resolve --mark --all
  (no more unresolved files)
  $ sl debugmergestate | grep 'merge driver:'
  merge driver: python:$TESTTMP/mergedriver-auto1.py (state "s")
  $ sl commit -m 'merged'

leave no files unresolved, but files driver-resolved
(r = False, unresolved = n, driver-resolved = y)

for the conclude step, also test that leaving files as driver-resolved
implicitly makes them resolved
  $ cat > ../mergedriver-driveronly.py << EOF
  > import bindings
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     mergestate.mark('foo.txt', 'd')
  >     mergestate.mark('bar.txt', 'd')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     bindings.commands.run(["hg", "debugmergestate"])
  >     mergestate.mark('foo.txt', 'r')
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-driveronly.py
  > EOF
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl merge 'desc(b)'
  * preprocess called
  * conclude called
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-driveronly.py (state "m")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "D", state "d", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "D", state "d", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)
  $ sl debugmergestate | grep 'merge driver:'
  merge driver: python:$TESTTMP/mergedriver-driveronly.py (state "s")
  $ sl commit -m 'merged'

indicate merge driver is necessary at commit
(r = True, unresolved = n, driver-resolved = n)

  $ cat > ../mergedriver-special.py << EOF
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     overrides = {('ui', 'forcemerge'): ':other'}
  >     with ui.configoverride(overrides, 'mergedriver'):
  >         mergestate.preresolve('foo.txt', wctx)
  >         mergestate.resolve('foo.txt', wctx)
  >         mergestate.preresolve('bar.txt', wctx)
  >         mergestate.resolve('bar.txt', wctx)
  >         mergestate.commit()
  > 
  >     return True
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     pass
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-special.py
  > EOF
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
XXX shouldn't output a warning
  $ sl merge 'desc(b)'
  * preprocess called
  warning: preprocess hook failed
  * conclude called
  0 files updated, 2 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)
  $ sl debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-special.py (state "s")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "r", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "r", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl commit -m 'merged'

make sure this works sensibly when files are unresolved
(r = True, unresolved = y, driver-resolved = n)

  $ cat > ../mergedriver-exit.py << EOF
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     return True
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     return True
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-exit.py
  > EOF
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
XXX shouldn't output a warning
  $ sl merge 'desc(b)'
  * preprocess called
  warning: preprocess hook failed
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  merging foo.txt
  warning: 1 conflicts while merging foo.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-exit.py (state "m")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "u", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "u", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl commit -m 'merged'
  abort: unresolved merge state
  (use 'sl resolve' to continue or
       'sl goto --clean' to abort - WARNING: will destroy uncommitted changes)
  [255]
  $ sl goto 'desc(c)'
  abort: unresolved merge state
  (use 'sl resolve' to continue or
       'sl goto --clean' to abort - WARNING: will destroy uncommitted changes)
  [255]

raise an error

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat > ../mergedriver-mark-and-raise.py << EOF
  > from sapling import error
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     for f in mergestate:
  >         mergestate.mark(f, 'r')
  >     mergestate.commit()
  >     raise error.Abort('foo')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     raise error.Abort('bar')
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-mark-and-raise.py
  > EOF

  $ sl merge 'desc(b)'
  * preprocess called
  error: preprocess hook failed: foo
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: foo
  warning: merge driver failed to preprocess files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-mark-and-raise.py (state "u")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "r", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "r", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl commit -m 'merged'
  abort: unresolved merge state
  (use 'sl resolve' to continue or
       'sl goto --clean' to abort - WARNING: will destroy uncommitted changes)
  [255]
  $ sl resolve --list
  R bar.txt
  R foo.txt

this shouldn't abort
  $ sl resolve --unmark --all
  * preprocess called
  error: preprocess hook failed: foo
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: foo
  warning: merge driver failed to preprocess files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  $ sl resolve --list
  U bar.txt
  U foo.txt

  $ sl resolve --mark --all --skip
  warning: skipping merge driver (you MUST regenerate artifacts afterwards)
  (no more unresolved files)
  $ sl debugmergestate | grep 'merge driver:'
  [1]

subsequent resolves shouldn't trigger the merge driver at all
  $ sl resolve --unmark --all
  $ sl resolve --mark --all
  (no more unresolved files)
  $ sl debugmergestate | grep 'merge driver:'
  merge driver: python:$TESTTMP/mergedriver-mark-and-raise.py (state "s")

this should go through at this point
  $ sl commit -m merged

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

  $ sl merge 'desc(b)'
  * preprocess called
  error: preprocess hook failed: foo
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: foo
  warning: merge driver failed to preprocess files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]

XXX this is really confused
  $ sl resolve --mark --all
  * preprocess called
  error: preprocess hook failed: foo
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: foo
  warning: merge driver failed to preprocess files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  * conclude called
  error: conclude hook failed: bar
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: bar
  warning: merge driver failed to resolve files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  [1]
  $ sl resolve --list
  R bar.txt
  R foo.txt

this should abort
  $ sl commit -m 'merged'
  abort: unresolved merge state
  (use 'sl resolve' to continue or
       'sl goto --clean' to abort - WARNING: will destroy uncommitted changes)
  [255]

this should disable the merge driver
  $ sl help resolve | grep -- 'skip'
      --skip                skip merge driver
  $ sl resolve --all --skip
  warning: skipping merge driver (you MUST regenerate artifacts afterwards)
  (no more unresolved files)

this should go through
  $ sl commit -m merged

this shouldn't invoke the merge driver
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

nor should this no-op update
  $ sl goto 'desc(c)'
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved

nor should this update with no working copy changes
  $ sl goto 'desc(b)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

test some other failure modes

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

  $ sl merge 'desc(b)' --config experimental.mergedriver=fail
  abort: merge driver must be a python hook
  [255]
  $ sl goto --clean 'desc(c)'
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
this should proceed as if there's no merge driver
  $ sl merge 'desc(b)' --config experimental.mergedriver=python:fail
  loading preprocess hook failed: [Errno 2] $ENOENT$: '$TESTTMP/repo1/fail'
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  merging foo.txt
  warning: 1 conflicts while merging foo.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl debugmergestate | grep 'merge driver:'
  merge driver: python:fail (state "s")
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cd ..
ensure the right path to load the merge driver hook
  $ sl -R repo1 merge 'desc(b)' --config experimental.mergedriver=python:fail
  loading preprocess hook failed: [Errno 2] $ENOENT$: '$TESTTMP/repo1/fail'
  merging repo1/bar.txt
  warning: 1 conflicts while merging repo1/bar.txt! (edit, then use 'sl resolve --mark')
  merging repo1/foo.txt
  warning: 1 conflicts while merging repo1/foo.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
verify behavior with different merge driver
  $ sl -R repo1 debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:fail (state "s")
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "u", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "u", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl -R repo1 resolve --mark --all --config experimental.mergedriver=
  (no more unresolved files)
  $ sl -R repo1 debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  labels:
    local: working copy
    other: merge rev
  file: bar.txt (record type "F", state "r", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "r", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl -R repo1 commit -m merged

this should invoke the merge driver
  $ cd repo1
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat > ../mergedriver-raise.py << EOF
  > from sapling import error
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     raise error.Abort('foo')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     raise error.Abort('bar')
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-raise.py
  > EOF
  $ echo foowd >> foo.txt
  $ sl goto ".^"
  * preprocess called
  error: preprocess hook failed: foo
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: foo
  warning: merge driver failed to preprocess files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  1 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges
  [1]
  $ sl debugmergestate
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  merge driver: python:$TESTTMP/mergedriver-raise.py (state "u")
  labels:
    local: working copy
    other: destination
  file: foo.txt (record type "F", state "u", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node 802224e80e899817a159d494c123fb421ac3efee)
    other path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    extras: ancestorlinknode=ede3d67b8d0fb0052854c85fb16823c825d21060
  $ sl resolve --list
  U foo.txt
XXX this is really confused
  $ sl resolve --mark --all
  * preprocess called
  error: preprocess hook failed: foo
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: foo
  warning: merge driver failed to preprocess files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  * conclude called
  error: conclude hook failed: bar
  Traceback (most recent call last):
    # collapsed by devel.collapse-traceback
  sapling.error.Abort: bar
  warning: merge driver failed to resolve files
  (sl resolve --all to retry, or sl resolve --all --skip to skip merge driver)
  [1]

test merge with automatic commit afterwards -- e.g. graft

  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-other.py
  > EOF
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl debugmergestate
  no merge state found
  $ sl graft 'desc(b)'
  grafting e0cfe070a2bb "b"
  local: ede3d67b8d0fb0052854c85fb16823c825d21060
  other: e0cfe070a2bbd0b727903026b7026cb0917e63b3
  merge driver: python:$TESTTMP/mergedriver-other.py (state "s")
  labels:
    local: local
    other: graft
  file: bar.txt (record type "F", state "r", hash 9d6caa30f54d05af0edb194bfa26137b109f2112)
    local path: bar.txt (flags "")
    ancestor path: bar.txt (node 4f30a68d92d62ca460d2c484d3fe4584c0521ae1)
    other path: bar.txt (node 18db82bb5e3b439444a63baf35364169e848cfd2)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  file: foo.txt (record type "F", state "r", hash 9206ac42b532ef8e983470c251f4e1a365fd636c)
    local path: foo.txt (flags "")
    ancestor path: foo.txt (node ad59c7ac23656632da079904d4d40d0bab4aeb80)
    other path: foo.txt (node 0b0743b512ba9b7c5db411597cf374a73b9f00ac)
    extras: ancestorlinknode=b9c4506f0639a99fcbfb8ce4764aa2aa4d2f6f92
  $ sl export
  # SL changeset patch
  # User test
  # Date 0 0
  #      Thu Jan 01 00:00:00 1970 +0000
  # Node ID 87ae466e19391befaaa0b92212ad70eef907404a
  # Parent  ede3d67b8d0fb0052854c85fb16823c825d21060
  b
  
  diff -r ede3d67b8d0f -r 87ae466e1939 bar.txt
  --- a/bar.txt	Thu Jan 01 00:00:00 1970 +0000
  +++ b/bar.txt	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,2 +1,2 @@
   abar
  -cbar
  +bbar
  diff -r ede3d67b8d0f -r 87ae466e1939 foo.txt
  --- a/foo.txt	Thu Jan 01 00:00:00 1970 +0000
  +++ b/foo.txt	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,2 +1,2 @@
   afoo
  -cfoo
  +bfoo

graft with failing merge

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-auto1.py
  > EOF
  $ sl graft e0cfe070a2bbd0b727903026b7026cb0917e63b3
  grafting e0cfe070a2bb "b"
  * preprocess called
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  abort: unresolved conflicts, can't continue
  (use 'sl resolve' and 'sl graft --continue')
  [255]
  $ sl resolve --list
  U bar.txt
  D foo.txt
  $ sl resolve --mark bar.txt
  (no more unresolved files -- run "sl resolve --all" to conclude)
  $ sl graft --continue
  grafting e0cfe070a2bb "b"
  abort: driver-resolved merge conflicts
  (run "sl resolve --all" to resolve)
  [255]
  $ sl resolve --unmark bar.txt
  $ sl resolve --list
  U bar.txt
  D foo.txt
  $ sl resolve foo.txt bar.txt --tool :other
  * conclude called
  (no more unresolved files)
  continue: sl graft --continue
XXX sl resolve --unmark --all doesn't cause the merge driver to be rerun
  $ sl resolve --mark --all
  (no more unresolved files)
  continue: sl graft --continue
  $ sl graft --continue
  grafting e0cfe070a2bb "b"
  $ sl export
  # SL changeset patch
  # User test
  # Date 0 0
  #      Thu Jan 01 00:00:00 1970 +0000
  # Node ID f22dab3f2e1c58088986931026ef5c22ba3f4006
  # Parent  ede3d67b8d0fb0052854c85fb16823c825d21060
  b
  
  diff -r ede3d67b8d0f -r f22dab3f2e1c bar.txt
  --- a/bar.txt	Thu Jan 01 00:00:00 1970 +0000
  +++ b/bar.txt	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,2 +1,2 @@
   abar
  -cbar
  +bbar
  diff -r ede3d67b8d0f -r f22dab3f2e1c foo.txt
  --- a/foo.txt	Thu Jan 01 00:00:00 1970 +0000
  +++ b/foo.txt	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,2 +1,2 @@
  -afoo
  -cfoo
  +abar
  +bbar

delete all the files

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat > ../mergedriver-delete.py << EOF
  > import os
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     ui.status('* preprocess called\n')
  >     for f in mergestate:
  >         if f in ["foo.txt", "bar.txt"]:
  >             mergestate.mark(f, 'd')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     ui.status('* conclude called\n')
  >     for f in mergestate.driverresolved():
  >         if f in ["foo.txt", "bar.txt"]:
  >             os.unlink(f)
  >             mergestate.queueremove(f)
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-delete.py
  > EOF
  $ sl graft e0cfe070a2bbd0b727903026b7026cb0917e63b3
  grafting e0cfe070a2bb "b"
  * preprocess called
  * conclude called
  $ sl status --change .
  R bar.txt
  R foo.txt
  $ f foo.txt bar.txt
  bar.txt: file not found
  foo.txt: file not found
  $ sl files
  [1]

delete all the files, but with a non-interactive conflict resolution involved
  $ sl revert --all
  $ sl up -q .^
  $ echo foo > other.txt
  $ sl commit -Aqm 'intro other.txt'
  $ echo bar > other.txt
  $ echo bar >> foo.txt
  $ sl commit -Aqm 'modify other.txt'
  $ sl up -q .^
  $ echo gah > other.txt
  $ echo gah >> foo.txt
  $ sl commit -Aqm 'different other.txt'
  $ sl --config extensions.rebase= rebase -d 'desc("modify other.txt")'
  rebasing f931f701d752 "different other.txt"
  * preprocess called
  merging other.txt
  warning: 1 conflicts while merging other.txt! (edit, then use 'sl resolve --mark')
  unresolved conflicts (see sl resolve, then sl rebase --continue)
  [1]
  $ echo gah > other.txt
  $ sl resolve --mark other.txt
  (no more unresolved files -- run "sl resolve --all" to conclude)
  $ sl resolve --all
  * conclude called
  (no more unresolved files)
  continue: sl rebase --continue
  $ sl st
  M other.txt
  R foo.txt
  ? other.txt.orig
  $ sl --config extensions.rebase= rebase --continue
  rebasing f931f701d752 "different other.txt"
  $ sl st
  ? other.txt.orig

conclude marks a file unresolved: merge must report it unresolved

A driver may need to leave a file unresolved from its conclude step (for
example, a strict mode that could not fully merge a file). Such a file must
be reported as unresolved to callers of the merge, rather than the merge
succeeding while the file remains "U" in the merge state.

  $ cd $TESTTMP
  $ cat > mergedriver-conclude-unresolved.py << EOF
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     for f in mergestate:
  >         mergestate.mark(f, 'd')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     mergestate.mark('foo.txt', 'u')
  > EOF
  $ sl init repo-conclude
  $ cd repo-conclude
  $ echo afoo > foo.txt
  $ echo abar > bar.txt
  $ sl add foo.txt bar.txt
  $ sl commit -ma
  $ echo bfoo >> foo.txt
  $ echo bbar >> bar.txt
  $ sl commit -mb
  $ sl up 'desc(a)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo cfoo >> foo.txt
  $ echo cbar >> bar.txt
  $ sl commit -mc
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-conclude-unresolved.py
  > EOF
  $ sl merge 'desc(b)'
  * preprocess called
  * conclude called
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl resolve --list
  R bar.txt
  U foo.txt
  $ sl resolve --mark foo.txt
  (no more unresolved files)
  $ sl commit -m merge

the recount is gated by experimental.mergedriver.recount-unresolved-after-conclude
(default on); with the gate off, the old behavior returns: the merge
reports success while the file remains unresolved

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl merge 'desc(b)' --config experimental.mergedriver.recount-unresolved-after-conclude=false
  * preprocess called
  * conclude called
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)
  $ sl resolve --list
  R bar.txt
  U foo.txt
  $ sl resolve --mark foo.txt
  (no more unresolved files)
  $ sl commit -m merge-gate-off

with the gate explicitly on, the merge reports the file unresolved again

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl merge 'desc(b)' --config experimental.mergedriver.recount-unresolved-after-conclude=true
  * preprocess called
  * conclude called
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl resolve --list
  R bar.txt
  U foo.txt
  $ sl resolve --mark foo.txt
  (no more unresolved files)
  $ sl commit -m merge-gate-on

conclude marks a file unresolved during a goto --merge

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo gfoo >> foo.txt
  $ sl goto --merge 'desc(b)'
  * preprocess called
  * conclude called
  1 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges
  [1]
  $ sl resolve --list
  U foo.txt
  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

conclude marks a file unresolved when run from resolve --all

In this case the merge itself leaves a different file unresolved, so the
conclude step is deferred until "sl resolve --all". That command must not
exit successfully while the driver has re-marked a file unresolved.

  $ cat > $TESTTMP/mergedriver-conclude-unresolved-resolve.py << EOF
  > def preprocess(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* preprocess called\n')
  >     mergestate.mark('foo.txt', 'd')
  > def conclude(ui, repo, hooktype, mergestate, wctx, labels):
  >     repo.ui.status('* conclude called\n')
  >     mergestate.mark('foo.txt', 'u')
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-conclude-unresolved-resolve.py
  > EOF
  $ sl merge 'desc(b)'
  * preprocess called
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl resolve bar.txt --tool internal:local
  (no more unresolved files -- run "sl resolve --all" to conclude)
  $ sl resolve --all
  * conclude called
  [1]
  $ sl resolve --list
  R bar.txt
  U foo.txt
  $ sl resolve --mark foo.txt
  (no more unresolved files)
  $ sl commit -m merge2

with the recount gate off, "sl resolve --all" exits successfully again
while the driver-re-marked file remains unresolved (the old behavior)

  $ sl goto --clean 'desc(c)'
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl merge 'desc(b)'
  * preprocess called
  merging bar.txt
  warning: 1 conflicts while merging bar.txt! (edit, then use 'sl resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges or 'sl goto -C .' to abandon
  [1]
  $ sl resolve bar.txt --tool internal:local
  (no more unresolved files -- run "sl resolve --all" to conclude)
  $ sl resolve --all --config experimental.mergedriver.recount-unresolved-after-conclude=false
  * conclude called
  $ sl resolve --list
  R bar.txt
  U foo.txt
  $ sl resolve --mark foo.txt
  (no more unresolved files)
  $ sl commit -m merge2-gate-off

conclude marks a file unresolved during rebase: rebase must stop for resolve

  $ cd $TESTTMP
  $ cat >> $HGRCPATH << EOF
  > [experimental]
  > mergedriver = python:$TESTTMP/mergedriver-conclude-unresolved.py
  > EOF
  $ sl init repo-rebase-conclude
  $ cd repo-rebase-conclude
  $ echo afoo > foo.txt
  $ sl add foo.txt
  $ sl commit -m base
  $ echo dfoo >> foo.txt
  $ sl commit -m dest
  $ sl up 'desc(base)'
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo sfoo >> foo.txt
  $ echo source > source.txt
  $ sl add source.txt
  $ sl commit -m source
  $ sl --config extensions.rebase= rebase -d 'desc(dest)'
  rebasing 03bef070aebf "source"
  * preprocess called
  * conclude called
  unresolved conflicts (see sl resolve, then sl rebase --continue)
  [1]
  $ sl resolve --list
  U foo.txt
  $ sl resolve --mark foo.txt
  (no more unresolved files)
  continue: sl rebase --continue
  $ sl --config extensions.rebase= rebase --continue
  rebasing 03bef070aebf "source"
  $ sl log -r . -T '{desc}\n'
  source
  $ cat source.txt
  source
