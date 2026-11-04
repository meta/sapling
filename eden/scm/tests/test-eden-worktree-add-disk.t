#require eden

One optional threshold controls the disk warning. Unset or zero means no
check; 9999TB is above the test machine's available disk, so it always warns.
Reported free/total figures vary by host and are globbed out.

setup backing repo

  $ newclientrepo diskrepo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"

test worktree add - no warning with the default unset threshold

  $ sl worktree add $TESTTMP/off1
  created linked worktree at $TESTTMP/off1

test worktree add - warns but still creates the worktree

  $ setconfig worktree.disk-warning-threshold=9999TB
  $ sl worktree add $TESTTMP/warn1
  low disk space: * remaining of * on '$TESTTMP/warn1'; continuing. To disable this warning, set `worktree.disk-warning-threshold=0` in your user config. (glob)
  created linked worktree at $TESTTMP/warn1
  $ test -d $TESTTMP/warn1

test worktree add - warning is repeated for each low-disk add

  $ sl worktree add $TESTTMP/warn2
  low disk space: * remaining of * on '$TESTTMP/warn2'; continuing. To disable this warning, set `worktree.disk-warning-threshold=0` in your user config. (glob)
  created linked worktree at $TESTTMP/warn2

test worktree add - agent warning remains advisory

  $ CODING_AGENT_METADATA=id=test_agent sl worktree add $TESTTMP/agentwarn
  low disk space: * remaining of * on '$TESTTMP/agentwarn'; continuing. This is advisory: low disk space did not block this worktree add; continue your task without asking the user about this warning. (glob)
  created linked worktree at $TESTTMP/agentwarn

test worktree add - zero turns warnings off again

  $ setconfig worktree.disk-warning-threshold=0
  $ sl worktree add $TESTTMP/off2
  created linked worktree at $TESTTMP/off2

test worktree add - invalid optional threshold does not block creation

  $ setconfig worktree.disk-warning-threshold=not-a-size
  $ sl worktree add $TESTTMP/badcfg
  created linked worktree at $TESTTMP/badcfg
