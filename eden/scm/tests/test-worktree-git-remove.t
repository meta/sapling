#require git no-windows no-eden

  $ . $TESTDIR/git.sh
  $ git init -qb main removals
  $ cd removals
  $ echo clean > file
  $ git add file
  $ git commit -qm init
  $ git worktree add -q $TESTTMP/clean --detach
  $ sl worktree label main-label
  label set for $TESTTMP/removals

Only exact linked checkout roots may be removed.

  $ sl worktree remove . -y
  abort: cannot remove the main worktree
  [255]
  $ mkdir $TESTTMP/clean/subdir
  $ sl worktree remove $TESTTMP/clean/subdir -y
  abort: $TESTTMP/clean/subdir is not the root of checkout $TESTTMP/clean, not removing
  [255]
  $ mkdir $TESTTMP/foreign
  $ sl worktree remove $TESTTMP/foreign -y
  abort: $TESTTMP/foreign is not in this worktree group
  [255]
  $ sl worktree remove $TESTTMP/clean $TESTTMP/clean -y
  removed $TESTTMP/clean
  $ sl worktree list
  * main    $TESTTMP/removals   main-label

Git keeps dirty and locked checkouts, including their labels.

  $ git worktree add -q $TESTTMP/dirty --detach
  $ git worktree add -q $TESTTMP/locked --detach
  $ sl worktree label $TESTTMP/dirty dirty-label
  label set for $TESTTMP/dirty
  $ sl worktree label $TESTTMP/locked locked-label
  label set for $TESTTMP/locked
  $ echo modified > $TESTTMP/dirty/file
  $ sl worktree remove $TESTTMP/dirty -y > $TESTTMP/remove-error 2>&1
  [255]
  $ grep -o 'contains modified or untracked files' $TESTTMP/remove-error
  contains modified or untracked files
  $ echo clean > $TESTTMP/dirty/file
  $ echo keep > $TESTTMP/dirty/untracked
  $ sl worktree remove $TESTTMP/dirty -y > $TESTTMP/remove-error 2>&1
  [255]
  $ cat $TESTTMP/dirty/untracked
  keep
  $ git worktree lock --reason keep $TESTTMP/locked
  $ sl worktree remove $TESTTMP/locked -y > $TESTTMP/remove-error 2>&1
  [255]
  $ grep -o 'cannot remove a locked working tree' $TESTTMP/remove-error
  cannot remove a locked working tree
  $ sl worktree list
  * main    $TESTTMP/removals   main-label
    linked  $TESTTMP/dirty   dirty-label
    linked  $TESTTMP/locked   locked-label

`--all` includes worktrees created with Git and keeps going after a failure.

  $ git worktree add -q $TESTTMP/another-clean --detach
  $ sl worktree remove --all -y > $TESTTMP/remove-error 2>&1
  [255]
  $ grep -o '^removed .*' $TESTTMP/remove-error
  removed $TESTTMP/another-clean
  removed 1 of 3 worktrees; 2 failed
  $ grep -o '^failed to remove [^:]*' $TESTTMP/remove-error
  failed to remove $TESTTMP/dirty
  failed to remove $TESTTMP/locked
  $ rm $TESTTMP/dirty/untracked
  $ git worktree unlock $TESTTMP/locked
  $ sl worktree remove --all -y
  removed $TESTTMP/dirty
  removed $TESTTMP/locked

The invoking checkout is removed last.

  $ git worktree add -q $TESTTMP/a-current --detach
  $ git worktree add -q $TESTTMP/z-other --detach
  $ (cd $TESTTMP/a-current && sl worktree remove --all -y --config 'hooks.pre-worktree-remove=echo HOOK:$HG_PATH')
  HOOK:$TESTTMP/z-other
  removed $TESTTMP/z-other
  HOOK:$TESTTMP/a-current
  removed $TESTTMP/a-current
  $ test ! -e $TESTTMP/removals/.git/worktrees
  $ sl worktree add $TESTTMP/self
  created linked worktree at $TESTTMP/self
  $ (cd $TESTTMP/self && sl worktree remove . -y)
  removed $TESTTMP/self
  $ sl worktree list
  * main    $TESTTMP/removals   main-label

`--force` removes a checkout with modified or untracked files, but not a locked one.

  $ git worktree add -q $TESTTMP/force-dirty --detach
  $ git worktree add -q $TESTTMP/force-locked --detach
  $ echo modified > $TESTTMP/force-dirty/file
  $ echo keep > $TESTTMP/force-dirty/untracked
  $ git worktree lock $TESTTMP/force-locked
  $ sl worktree remove $TESTTMP/force-dirty $TESTTMP/force-locked -y --force > $TESTTMP/remove-error 2>&1
  [255]
  $ grep -o '^removed .*' $TESTTMP/remove-error
  removed $TESTTMP/force-dirty
  removed 1 of 2 worktrees; 1 failed
  $ grep -o '^failed to remove [^:]*' $TESTTMP/remove-error
  failed to remove $TESTTMP/force-locked

Shelved changes in a Git-created worktree require --force.

  $ git worktree add -q $TESTTMP/shelved --detach
  $ echo modified > $TESTTMP/shelved/file
  $ sl --cwd $TESTTMP/shelved shelve -q --config extensions.shelve=
  $ sl --cwd $TESTTMP/shelved status
  $ sl worktree remove $TESTTMP/shelved -y
  abort: $TESTTMP/shelved has shelved changes, use --force to remove it
  [255]
  $ sl worktree remove $TESTTMP/shelved -y --force
  removed $TESTTMP/shelved

Removal works from a Git-created worktree before Sapling has run in the main checkout.

  $ cd $TESTTMP
  $ git init -qb main fresh
  $ cd fresh
  $ echo clean > file
  $ git add file
  $ git commit -qm init
  $ git worktree add -q $TESTTMP/fresh-current --detach
  $ git worktree add -q $TESTTMP/fresh-other --detach
  $ test ! -e $TESTTMP/fresh/.git/sl
  $ (cd $TESTTMP/fresh-current && sl worktree remove $TESTTMP/fresh-other -y)
  removed $TESTTMP/fresh-other
