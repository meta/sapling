# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from sapling import extensions, scmutil
from sapling.ext.github import github_gh_cli, submit
from sapling.ext.github.mock_utils import (
    mock_run_git_command,
    MockGitHubServer,
    wrap_with_consumption_check,
)
from sapling.ext.github.pull_request_body import title_and_body

# An extension to mock network requests for `sl pr submit` with
# github.placeholder-strategy=true and github.pr-workflow=single when the
# repo is a fork. Chained bases are not possible for forks (the head branches
# live on the fork, but a pull request's base branch must be a branch on the
# upstream repository), so both new pull requests must be created against the
# upstream default branch, and their body rewrites must not attempt to change
# the base to a fork branch.

UPSTREAM = {
    "id": "R_upstream_repo",
    "owner": {"id": "upstream_id", "login": "upstream"},
    "name": "test_github_repo",
    "isFork": False,
    "defaultBranchRef": {"name": "main"},
}


def setup_mock_github_server(repo) -> MockGitHubServer:
    github_server = MockGitHubServer()

    github_server.expect_get_repository_request().and_respond(
        is_fork=True, parent=UPSTREAM
    )

    # Placeholder issues are reserved on the upstream repository.
    github_server.expect_create_pr_placeholder_request(
        owner="upstream"
    ).and_respond(start_number=42, num_times=2)

    prs = [
        (42, "one\n"),
        (43, "two\n"),
    ]
    for num, msg in prs:
        _title, body = title_and_body(msg)
        # Despite the "single" workflow, both pull requests use the upstream
        # default branch as the base: fork head branches cannot be bases.
        github_server.expect_create_pr_using_placeholder_request(
            body=body,
            issue=num,
            head=f"facebook:pr{num}",
            base="main",
            owner="upstream",
        ).and_respond()

        pr_id = f"PR_id_{num}"
        github_server.expect_get_pr_details_request(
            num, owner="upstream"
        ).and_respond(pr_id)

        # The body rewrite keeps the default base branch (no chaining).
        github_server.expect_update_pr_request(
            pr_id,
            num,
            msg,
            base="main",
            owner="upstream",
            stack_pr_ids=[42, 43],
        ).and_respond()

    github_server.expect_get_username_request().and_respond()

    tip = scmutil.revsingle(repo, "desc(two)").hex()
    github_server.expect_merge_into_branch(tip).and_respond()

    return github_server


def reposetup(ui, repo):
    github_server = setup_mock_github_server(repo)
    extensions.wrapfunction(
        github_gh_cli, "_make_request", github_server.make_request
    )
    extensions.wrapfunction(submit, "run_git_command", mock_run_git_command)
    wrap_with_consumption_check(github_server, submit, "submit")
