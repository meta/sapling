# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from sapling import extensions, scmutil
from sapling.ext.github import github_gh_cli, submit
from sapling.ext.github.gh_submit import PullRequestState
from sapling.ext.github.mock_utils import (
    mock_run_git_command,
    MockGitHubServer,
    wrap_with_consumption_check,
)

# An extension to mock network requests for `sl pr submit` with
# github.pr-workflow=single when the pull request at the bottom of the stack
# (#42) is closed: the base branches of the pull requests above it must skip
# the closed pull request's head branch (which would break the chain) and
# fall through to the default base branch.
#
# This mock is set up in `reposetup` so that expectations can be derived from
# the actual commit hashes in the test repo instead of hardcoding them.


def setup_mock_github_server(repo) -> MockGitHubServer:
    github_server = MockGitHubServer()

    github_server.expect_get_repository_request().and_respond()

    # All three commits are linked to pull requests via "Pull Request
    # resolved" lines in their commit messages. None of the head OIDs match
    # the local commits, so all three head branches are pushed (pushing to a
    # closed pull request's branch is harmless and preexisting behavior).
    github_server.expect_get_pr_details_request(42).and_respond(
        "PR_id_42", state=PullRequestState.CLOSED
    )
    github_server.expect_get_pr_details_request(43).and_respond("PR_id_43")
    github_server.expect_get_pr_details_request(44).and_respond("PR_id_44")

    # Base updates before the push: #44 chains to the open #43 below it, but
    # #43 must NOT chain to the closed #42 below it: it falls through to the
    # default base branch instead. (No base update is attempted for the
    # closed #42 itself.)
    github_server.expect_update_pr_request(
        "PR_id_44", 44, "", base="pr43"
    ).and_respond()
    github_server.expect_update_pr_request(
        "PR_id_43", 43, "", base="main"
    ).and_respond()

    # Body rewrites follow the same base rules. The stack list footer still
    # lists all three pull requests (including the closed one).
    msg_two = "two\n\nPull Request resolved: https://github.com/facebook/test_github_repo/pull/43"
    msg_three = "three\n\nPull Request resolved: https://github.com/facebook/test_github_repo/pull/44"
    github_server.expect_update_pr_request(
        "PR_id_44", 44, msg_three, base="pr43", stack_pr_ids=[42, 43, 44]
    ).and_respond()
    github_server.expect_update_pr_request(
        "PR_id_43", 43, msg_two, base="main", stack_pr_ids=[42, 43, 44]
    ).and_respond()

    github_server.expect_get_username_request().and_respond()

    tip = scmutil.revsingle(repo, "desc(three)").hex()
    github_server.expect_merge_into_branch(tip).and_respond()

    return github_server


def reposetup(ui, repo):
    github_server = setup_mock_github_server(repo)
    extensions.wrapfunction(
        github_gh_cli, "_make_request", github_server.make_request
    )
    extensions.wrapfunction(submit, "run_git_command", mock_run_git_command)
    wrap_with_consumption_check(github_server, submit, "submit")
