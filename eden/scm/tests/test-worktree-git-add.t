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

Without PATH or a generator, worktrees go next to the main checkout, skipping
names that exist on disk or that Git still has registered.

  $ sl worktree add
  created linked worktree at \$TESTTMP/addrepo\.worktrees/[a-z0-9]{8} (re)
  $ sl worktree add --label "Review X"
  created linked worktree at $TESTTMP/addrepo.worktrees/review-x
  $ sl worktree add --label review-x
  created linked worktree at $TESTTMP/addrepo.worktrees/review-x-2
  $ rm -rf $TESTTMP/addrepo.worktrees/review-x-2
  $ sl worktree add --label review-x
  created linked worktree at $TESTTMP/addrepo.worktrees/review-x-3

Long labels are cut to 32 characters, without a trailing `-`.

  $ sl worktree add --label "Fix a slow test in the worktree add command when the label is too long"
  created linked worktree at $TESTTMP/addrepo.worktrees/fix-a-slow-test-in-the-worktree

Another add with the same label can run while this add holds its chosen
name; the pre-add hook stands in for it. That add skips to the next name, and
the hook already sees this add's final path. With `worktree.max-count` set, this
add also reserves its slot while it holds the name.

  $ sl worktree add --label race --config worktree.max-count=20 --config 'hooks.pre-worktree-add=echo $HG_PATH; sl worktree add --label race'
  $TESTTMP/addrepo.worktrees/race
  created linked worktree at $TESTTMP/addrepo.worktrees/race-2
  created linked worktree at $TESTTMP/addrepo.worktrees/race

An explicit PATH doesn't skip ahead: it aborts if another add created the path
first.

  $ sl worktree add $TESTTMP/taken --config 'hooks.pre-worktree-add=sl worktree add "$HG_PATH"'
  created linked worktree at $TESTTMP/taken
  abort: destination path '$TESTTMP/taken' already exists
  [255]
