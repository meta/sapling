#require git no-eden no-windows

  $ eagerepo
  $ enable github
  $ export SL_TEST_GH_URL=https://github.com/facebook/test_github_repo.git
  $ . $TESTDIR/git.sh
  $ setconfig github.pr-workflow=single

build up a github repo

  $ sl init --git repo1
  $ cd repo1
  $ echo a > a1
  $ sl ci -Aqm "Pull Request resolved: https://github.com/facebook/test_github_repo/pull/42"

test we don't try updating a closed pr:

  $ sl pr submit --config extensions.pr_submit=$TESTDIR/github/mock_existing_closed_pr.py
  pushing 1 to https://github.com/facebook/test_github_repo.git
  warning, not updating #42 because it isn't open
  hint[unlink-closed-pr]: to create a new PR, disassociate commit(s) using 'sl pr unlink' then re-run 'sl pr submit'
