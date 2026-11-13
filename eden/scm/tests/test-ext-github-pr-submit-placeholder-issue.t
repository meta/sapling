#require git no-eden no-windows

  $ eagerepo
  $ enable github
  $ export SL_TEST_GH_URL=https://github.com/facebook/test_github_repo.git
  $ . $TESTDIR/git.sh

build up a github repo

  $ sl init --git repo1
  $ cd repo1
  $ setconfig github.placeholder-strategy=True
  $ echo a > a1
  $ sl ci -Am addfile
  adding a1

confirm it is a 'github_repo'
  $ sl log -r. -T '{github_repo}\n'
  True

test sending pr
  $ sl pr submit --config extensions.pr_submit=$TESTDIR/github/mock_create_one_pr_placeholder_issue.py
  pushing 1 to https://github.com/facebook/test_github_repo.git
  created new pull request: https://github.com/facebook/test_github_repo/pull/1
  updated body for https://github.com/facebook/test_github_repo/pull/1

test the placeholder strategy with a chained-bases workflow on a fork: fork
head branches cannot be used as base branches on the upstream repository, so
both pull requests are created against the upstream default branch

  $ cd ..
  $ sl init --git repo2
  $ cd repo2
  $ setconfig github.placeholder-strategy=True
  $ setconfig github.pr-workflow=single
  $ echo a > a1
  $ sl ci -Aqm one
  $ echo a >> a1
  $ sl ci -Aqm two
  $ sl pr submit --config extensions.pr_submit=$TESTDIR/github/mock_create_prs_placeholder_fork.py
  pushing 2 to https://github.com/facebook/test_github_repo.git
  created new pull request: https://github.com/upstream/test_github_repo/pull/42
  created new pull request: https://github.com/upstream/test_github_repo/pull/43
  updated body for https://github.com/upstream/test_github_repo/pull/43
  updated body for https://github.com/upstream/test_github_repo/pull/42
