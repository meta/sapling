#require git no-windows no-eden

  $ . $TESTDIR/git.sh
  $ git init -qb main addrepo
  $ cd addrepo
  $ echo first > file
  $ git add file
  $ git commit -qm first
  $ git tag first
  $ echo second > file
  $ git commit -qam second
  $ git worktree add -q $TESTTMP/native -b feature first

Adding from a Git-created linked checkout uses its HEAD and initializes Sapling.

  $ cd $TESTTMP/native
  $ sl worktree add "$TESTTMP/sl linked" --label "new label"
  created linked worktree at $TESTTMP/sl linked
  $ git -C "$TESTTMP/sl linked" symbolic-ref -q HEAD
  [1]
  $ git -C "$TESTTMP/sl linked" log -1 --format=%s
  first
  $ cd "$TESTTMP/sl linked"
  $ sl status
  $ sl worktree list
    main    $TESTTMP/addrepo
    linked  $TESTTMP/native
  * linked  $TESTTMP/sl linked   new label

An explicit revision is resolved by Sapling.

  $ sl worktree add $TESTTMP/at-main -r "$(git rev-parse main)"
  created linked worktree at $TESTTMP/at-main
  $ git -C $TESTTMP/at-main log -1 --format=%s
  second

Snapshots are rejected before generators or pre-add hooks execute.

  $ sl worktree add --snapshot --config 'worktree.path-generator=touch "$TESTTMP/generator-ran"' --config 'hooks.pre-worktree-add=touch "$TESTTMP/hook-ran"'
  abort: --snapshot is not supported for Git worktrees
  [255]
  $ test ! -e $TESTTMP/generator-ran
  $ test ! -e $TESTTMP/hook-ran

The limit includes raw Git worktrees.

  $ sl worktree add $TESTTMP/over-limit --config worktree.max-count=3
  abort: cannot create worktree: limit of 3 linked worktrees reached for repository '$TESTTMP/sl linked' (currently 3); set `worktree.max-count` in your user config (e.g. ~/.hgrc) to raise it, or 0 to disable the limit
  [255]

A revset expression is resolved by Sapling too.

  $ sl worktree add $TESTTMP/at-parent -r "$(git rev-parse main)^"
  created linked worktree at $TESTTMP/at-parent
  $ git -C $TESTTMP/at-parent log -1 --format=%s
  first

A revset returning multiple commits must select one explicitly.

  $ revset="$(git rev-parse main)^ + $(git rev-parse main)"
  $ sl worktree add $TESTTMP/multiple-revs -r "$revset"
  abort: revision set contains multiple commits; use first(...), last(...), or a commit hash
  [255]
  $ test -d $TESTTMP/multiple-revs
  [1]
  $ sl worktree add $TESTTMP/first-rev -r "first($revset)"
  created linked worktree at $TESTTMP/first-rev
  $ git -C $TESTTMP/first-rev log -1 --format=%s
  first
  $ sl worktree add $TESTTMP/last-rev -r "last($revset)"
  created linked worktree at $TESTTMP/last-rev
  $ git -C $TESTTMP/last-rev log -1 --format=%s
  second

Adding from a linked checkout inherits its sparse patterns and worktree configuration.

  $ cd $TESTTMP/native
  $ git sparse-checkout set --no-cone '!/*'
  $ git config --worktree test.source linked
  $ test ! -e file
  $ sl worktree add $TESTTMP/sparse-child
  created linked worktree at $TESTTMP/sparse-child
  $ test ! -e $TESTTMP/sparse-child/file
  $ git -C $TESTTMP/sparse-child config --worktree --get test.source
  linked
  $ git -C $TESTTMP/sparse-child config --worktree --get core.sparseCheckout
  true
