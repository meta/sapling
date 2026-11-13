# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from sapling import extensions
from sapling.ext.github import github_gh_cli, submit
from sapling.ext.github.consts import GITHUB_HOSTNAME
from sapling.ext.github.mock_utils import (
    mock_run_git_command,
    MockGitHubServer,
    OWNER,
    REPO_NAME,
)
from sapling.ext.github.pull_request_body import (
    _format_review_url,
    DEFAULT_REVIEW_TOOL_NAME,
    DEFAULT_REVIEW_URL_TEMPLATE,
    title_and_body,
)

# An extension to mock network requests by replacing the `github_gh_cli.make_request`
# and `submit.run_git_command` with the corresponding wrapper functions. Check `uisetup`
# function for how wrapper functions are registered.


def setup_mock_github_server(ui) -> MockGitHubServer:
    """Setup mock GitHub Server for testing happy case of `sl pr submit` command."""
    github_server = MockGitHubServer()

    github_server.expect_get_repository_request().and_respond()

    github_server.expect_guess_next_pull_request_number().and_respond()

    prs = [
        (42, "one\n"),
        (43, "two\n"),
    ]

    # Both "single" and "stacked" chain each PR's base to the PR below it.
    chained = ui.config("github", "pr-workflow") in ("single", "stacked")

    for idx, (num, msg) in enumerate(prs):
        title, body = title_and_body(msg)
        head = f"pr{num}"

        base = "main"
        if chained and idx > 0:
            base = "pr%d" % prs[idx - 1][0]

        github_server.expect_create_pr_request(
            body=body,
            title=title,
            head=head,
            base=base,
        ).and_respond(number=num)

        pr_id = f"PR_id_{num}"
        github_server.expect_get_pr_details_request(num).and_respond(pr_id)

        github_server.expect_update_pr_request(
            pr_id,
            num,
            msg,
            base=base,
            stack_pr_ids=[pr[0] for pr in prs],
            review_url=_format_review_url(
                DEFAULT_REVIEW_URL_TEMPLATE,
                owner=OWNER,
                repo=REPO_NAME,
                number=num,
                hostname=GITHUB_HOSTNAME,
            ),
            review_tool=DEFAULT_REVIEW_TOOL_NAME,
        ).and_respond()

    github_server.expect_get_username_request().and_respond()

    head = "1a67244b0a776bfcc3be6bf811e98c993d78ce47"
    github_server.expect_merge_into_branch(head).and_respond()

    return github_server


def uisetup(ui):
    mock_github_server = setup_mock_github_server(ui)
    extensions.wrapfunction(
        github_gh_cli, "_make_request", mock_github_server.make_request
    )
    extensions.wrapfunction(submit, "run_git_command", mock_run_git_command)
