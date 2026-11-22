#require git no-eden no-windows

  $ eagerepo
  $ enable github
  $ export SL_TEST_GH_URL=https://github.com/facebook/test_github_repo.git
  $ . $TESTDIR/git.sh
  $ configure github.pr-workflow=single

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

test chained bases skip a closed pull request: two open pull requests are
stacked on top of the closed #42. #43 must NOT use the closed #42's head
branch as its base (that would break the chain); it falls through to the
default base branch instead.

  $ echo b > b1
  $ sl ci -Aqm "two
  > 
  > Pull Request resolved: https://github.com/facebook/test_github_repo/pull/43"
  $ echo c > c1
  $ sl ci -Aqm "three
  > 
  > Pull Request resolved: https://github.com/facebook/test_github_repo/pull/44"
  $ sl pr submit --config extensions.pr_submit=$TESTDIR/github/mock_closed_mid_stack.py
  updated base for https://github.com/facebook/test_github_repo/pull/44
  updated base for https://github.com/facebook/test_github_repo/pull/43
  pushing 3 to https://github.com/facebook/test_github_repo.git
  updated body for https://github.com/facebook/test_github_repo/pull/44
  updated body for https://github.com/facebook/test_github_repo/pull/43
  warning, not updating #42 because it isn't open
  hint[unlink-closed-pr]: to create a new PR, disassociate commit(s) using 'sl pr unlink' then re-run 'sl pr submit'
