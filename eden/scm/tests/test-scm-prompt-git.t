#require bash no-eden

  $ eagerepo
Initialize scm prompt
  $ . $TESTDIR/../contrib/scm-prompt.sh

  $ cmd() {
  >   "$@"
  >   _scm_prompt "(%s)\n"
  > }
  $ git() {
  >   command git -c init.defaultBranch=master "$@"
  > }

  $ quietcmd() {
  >   "$@" &> /dev/null
  >   _scm_prompt "(%s)\n"
  > }

Set env to make commands repeatable

  $ GIT_AUTHOR_NAME="Test User"
  $ export GIT_AUTHOR_NAME
  $ GIT_AUTHOR_EMAIL="test@user.org"
  $ export GIT_AUTHOR_EMAIL
  $ GIT_AUTHOR_DATE="2006-07-03 17:18:43 +0200"
  $ export GIT_AUTHOR_DATE
  $ GIT_COMMITTER_NAME="Test User"
  $ export GIT_COMMITTER_NAME
  $ GIT_COMMITTER_EMAIL="test@user.org"
  $ export GIT_COMMITTER_EMAIL
  $ GIT_COMMITTER_DATE="2006-07-03 17:18:43 +0200"
  $ export GIT_COMMITTER_DATE

Outside of a repo, should have no output
  $ _scm_prompt

Set up repo
  $ git init -q repo
  $ cd repo
  $ _scm_prompt "%s\n"
  master
  $ cmd git commit -q --allow-empty -m "Initial empty commit"
  (master)

Test basics
  $ echo a > a
  $ git add a
  $ cmd git commit -qm "aa"
  (master)
  $ cmd git checkout -q "HEAD^"
  (1fed3389)

Test rebase
  $ cmd git checkout -q -b work "master^"
  (work)
  $ echo b > a
  $ git add a
  $ cmd git commit -qm "ba"
  (work)
  $ quietcmd git rebase master
  (eef45076|REBASE*|work) (glob)
  $ cmd git rebase --abort
  (work)
  $ quietcmd git rebase --merge master
  (eef45076|REBASE-*|work) (glob)
  $ cmd git rebase --abort
  (work)

Test more advanced workflows
  $ git format-patch "HEAD^" --stdout > .git/patch-ba
  $ quietcmd git am < .git/patch-ba
  (work|AM)
  $ cmd git am --abort
  (work)
  $ echo c > a
  $ cmd git commit -qam "ca"
  (work)

  $ quietcmd git revert eef4507
  (work|REVERTING)
  $ cmd git revert --abort
  (work)

  $ quietcmd git cherry-pick eef4507
  (work|CHERRY-PICKING)
  $ cmd git cherry-pick --abort
  (work)

  $ quietcmd git merge eef4507
  (work|MERGE)
  $ cmd git merge --abort
  (work)

  $ quietcmd git bisect start
  (work|BISECT)
  $ cmd git bisect reset
  Already on 'work'
  (work)

A reftable repo keeps a placeholder in the HEAD file
  $ cd ..
  $ git init -q --ref-format=reftable reftable
  $ cd reftable
  $ cat .git/HEAD
  ref: refs/heads/.invalid
  $ _scm_prompt "(%s)\n"
  (master)
  $ cmd git commit -q --allow-empty -m "Initial empty commit"
  (master)
  $ quietcmd git checkout --detach
  (1fed3389)
