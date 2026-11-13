#require git no-eden no-windows

Test that `sl pr submit --open` opens created PRs in the browser.

  $ eagerepo
  $ enable github
  $ export SL_TEST_GH_URL=https://github.com/facebook/test_github_repo.git
  $ . $TESTDIR/git.sh
  $ setconfig github.pr-workflow=overlap

build up a github repo

  $ sl init --git repo1
  $ cd repo1
  $ echo a > a1
  $ sl ci -Aqm one
  $ echo a >> a1
  $ sl ci -Aqm two

confirm it is a 'github_repo'
  $ sl log -r. -T '{github_repo}\n'
  True

test sending pr with --open flag
  $ sl pr submit --open --config extensions.pr_submit=$TESTDIR/github/mock_create_prs_with_open.py
  pushing 2 to https://github.com/facebook/test_github_repo.git
  created new pull request: https://github.com/facebook/test_github_repo/pull/42
  created new pull request: https://github.com/facebook/test_github_repo/pull/43
  updated body for https://github.com/facebook/test_github_repo/pull/43
  updated body for https://github.com/facebook/test_github_repo/pull/42
  opening https://github.com/facebook/test_github_repo/pull/43
  opening https://github.com/facebook/test_github_repo/pull/42
  [mock] opened browser: https://github.com/facebook/test_github_repo/pull/43
  [mock] opened browser: https://github.com/facebook/test_github_repo/pull/42
