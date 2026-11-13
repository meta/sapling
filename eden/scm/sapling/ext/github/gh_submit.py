# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""logic for submit.py implemented by shelling out to the GitHub CLI.

Ultimately, we expect to replace this with a Rust implementation that makes
the API calls directly so we can (1) avoid spawning so many processes, and
(2) do more work in parallel.
"""

import enum
from dataclasses import dataclass
from typing import Dict, List, Optional, Tuple, Union

from sapling.i18n import _
from sapling.result import Err, Ok, Result

from . import github_gh_cli as gh_cli
from .consts import query
from .github_gh_cli import JsonDict, ParamValue
from .pullrequest import PullRequestId

_Params = Union[str, int, bool]


@dataclass
class Repository:
    # ID for the repository for use with other GitHub API calls.
    id: str
    # If GitHub Enterprise, this is the Enterprise hostname; otherwise, it is
    # "github.com".
    hostname: str
    # In GitHub, a "RepositoryOwner" is either an "Organization" or a "User":
    # https://docs.github.com/en/graphql/reference/interfaces#repositoryowner
    owner: str
    # Name of the GitHub repo within the organization.
    name: str
    # Name of the default branch.
    default_branch: str
    # True if this is a fork.
    is_fork: bool
    # Should be set if is_fork is True, though if this is a fork of a fork,
    # then we only traverse one link in the chain, so this could still be None.
    upstream: Optional["Repository"] = None

    def get_base_branch(self) -> str:
        """If this is a fork, returns the default_branch of the upstream repo."""
        if self.upstream:
            return self.upstream.default_branch
        else:
            return self.default_branch

    def get_upstream_owner_and_name(self) -> Tuple[str, str]:
        """owner and name to use when creating a pull request"""
        if self.upstream:
            return (self.upstream.owner, self.upstream.name)
        else:
            return (self.owner, self.name)


async def get_repository(
    hostname: str, owner: str, name: str
) -> Result[Repository, str]:
    """Returns an "ID!" for the repository that is necessary in other
    GitHub API calls.
    """
    params: Dict[str, _Params] = {
        "query": query.GRAPHQL_GET_REPOSITORY,
        "owner": owner,
        "name": name,
    }
    result = await gh_cli.make_request(params, hostname=hostname)
    if result.is_err():
        return Err(result.unwrap_err())

    data = result.unwrap()["data"]
    repo = data["repository"]
    parent = repo["parent"]

    if parent:
        result = _parse_repository_from_dict(parent, hostname=hostname)
        if result.is_err():
            return result
        else:
            upstream = result.unwrap()
    else:
        upstream = None
    return _parse_repository_from_dict(repo, hostname=hostname, upstream=upstream)


class PullRequestState(enum.Enum):
    """https://docs.github.com/en/graphql/reference/enums#pullrequeststate"""

    """A pull request that has been closed without being merged."""
    CLOSED = enum.auto()
    """A pull request that has been closed by being merged."""
    MERGED = enum.auto()
    """A pull request that is still open."""
    OPEN = enum.auto()


@dataclass
class PullRequestDetails:
    node_id: str
    number: int
    url: str
    base_oid: str
    base_branch_name: str
    head_oid: str
    head_branch_name: str
    # body should be the pull request body as authored by the user (i.e.,
    # containing Markdown source), as opposed to:
    #   bodyText: plaintext version of body with Markdown markup removed
    #   bodyHTML: body rendered as "safe" HTML
    body: str
    title: str
    state: PullRequestState


async def get_pull_request_details(
    pr: PullRequestId,
) -> Result[PullRequestDetails, str]:
    params = {
        "query": query.GRAPHQL_GET_PULL_REQUEST,
        "owner": pr.owner,
        "name": pr.name,
        "number": pr.number,
    }
    result = await gh_cli.make_request(params, hostname=pr.get_hostname())
    if result.is_err():
        return Err(result.unwrap_err())

    data = result.unwrap()["data"]["repository"]["pullRequest"]
    return Ok(
        PullRequestDetails(
            node_id=data["id"],
            number=pr.number,
            url=data["url"],
            base_oid=data["baseRefOid"],
            base_branch_name=data["baseRefName"],
            head_oid=data["headRefOid"],
            head_branch_name=data["headRefName"],
            body=data["body"],
            title=data["title"],
            state=PullRequestState[data["state"]],
        )
    )


def _parse_repository_from_dict(
    repo_obj, hostname: str, upstream=None
) -> Result[Repository, str]:
    owner = repo_obj["owner"]["login"]
    name = repo_obj["name"]
    branch_ref = repo_obj["defaultBranchRef"]
    if branch_ref is None:
        error_message = (
            _(
                """\
This repository has no default branch. This is likely because it is empty.

Consider using %s to initialize your
repository.
"""
            )
            % f"https://{hostname}/{owner}/{name}/new/main"
        )
        return Err(error_message)
    return Ok(
        Repository(
            id=repo_obj["id"],
            hostname=hostname,
            owner=owner,
            name=name,
            default_branch=branch_ref["name"],
            is_fork=repo_obj["isFork"],
            upstream=upstream,
        )
    )


async def guess_next_pull_request_number(
    hostname: str, owner: str, name: str
) -> Result[int, str]:
    """Returns our best guess as to the number that will be assigned to the next
    pull request for the specified repo. It is a "guess" because it is based
    on the largest number for either issues or pull requests seen thus far and
    adds 1 to it. This "guess" can be wrong if:

    - The most recent pull request/issue has been deleted, in which case the
      next number would be one more than that.
    - If an issue/pull request is created between the time this function is
      called and the pull request is created, the guess will also be wrong.

    Note that the only reason we bother to do this is because, at least at the
    time of this writing, we cannot rename  the branch used for the head of a
    pull request [programmatically] without closing the pull request.

    While there is an official GitHub API for renaming a branch, it closes all
    pull requests that have their `head` set to the old branch name!
    Unfortunately, this is not documented on:

    https://docs.github.com/en/rest/branches/branches#rename-a-branch

    Support for renaming a branch WITHOUT closing all of the pull requests was
    introduced in Jan 2021, but it only appears to be available via the Web UI:

    https://github.blog/changelog/2021-01-19-support-for-renaming-an-existing-branch/

    The endpoint the web UI hits is on github.com, not api.github.com, so it
    does not appear to be accessible to us.
    """
    params: Dict[str, _Params] = {
        "query": query.GRAPHQL_GET_MAX_PR_ISSUE_NUMBER,
        "owner": owner,
        "name": name,
    }
    result = await gh_cli.make_request(params, hostname=hostname)
    if result.is_err():
        return Err(result.unwrap_err())

    # Find the max value of the fields, though note that it is possible no
    # issues or pull requests have ever been filed.
    repository = result.unwrap()["data"]["repository"]

    def get_value(field):
        nodes = repository[field]["nodes"]
        return nodes[0]["number"] if nodes else 0

    values = [get_value(field) for field in ["issues", "pullRequests"]]
    next_number = max(*values) + 1
    return Ok(next_number)


async def create_pull_request(
    hostname: str,
    owner: str,
    name: str,
    base: str,
    head: str,
    title: str,
    body: str,
    is_draft: bool = False,
) -> Result:
    """Creates a new pull request using the specified parameters.

    The caller is responsible for ensuring that a non-zero set of commits exists
    between `base` and `head`. See https://github.com/facebook/sapling/issues/384.
    """
    endpoint = f"repos/{owner}/{name}/pulls"
    params: Dict[str, _Params] = {
        "base": base,
        "head": head,
        "title": title,
        "body": body,
        "draft": is_draft,
    }
    return await gh_cli.make_request(params, hostname=hostname, endpoint=endpoint)


async def create_pull_request_placeholder_issue(
    hostname: str,
    owner: str,
    name: str,
) -> Result[int, str]:
    """creates a GitHub issue for the purpose of reserving an issue number"""
    endpoint = f"repos/{owner}/{name}/issues"
    params: Dict[str, _Params] = {
        "title": "placeholder for pull request",
    }
    result = await gh_cli.make_request(params, hostname=hostname, endpoint=endpoint)
    if result.is_err():
        return Err(result.unwrap_err())
    else:
        return Ok(result.unwrap()["number"])


async def create_pull_request_from_placeholder_issue(
    hostname: str,
    owner: str,
    name: str,
    base: str,
    head: str,
    body: str,
    issue: int,
    is_draft: bool = False,
) -> Result[JsonDict, str]:
    """Creates a new pull request by converting an existing issue into a PR.

    The caller is responsible for ensuring that a non-zero set of commits exists
    between `base` and `head`. See https://github.com/facebook/sapling/issues/384.

    Note that `title` and `issue` are mutually exclusive fields when creating a
    pull request.

    Note that the documented HTTP response status codes
    (https://docs.github.com/en/rest/pulls/pulls?apiVersion=2022-11-28#create-a-pull-request--status-codes)
    for this REST endpoint are:

    201 Created
    403 Forbidden
    422 Validation failed, or the endpoint has been spammed.

    In the event of a failure, *ideally* we would close or delete the
    placeholder issue (or even better, save it for later use), but that seems
    tricky do here because:

    403 If creating a PR for the issue is forbidden, closing it probably is, too.
    422 If the endpoint has been spammed, then it seems unlikely that making
        *another* request to the endpoint to close the issue will succeed.

    Though https://github.com/facebook/sapling/issues/371 revealed that some
    repos opt to disable GitHub issues. Enabling issues should not be a
    requirement for creating pull requests, so the "placeholder issue" scheme is
    a non-starter for such repos.

    TODO: Figure out some sort of error-recovery scheme. Note that
    make_request() returns an error as a string that may or may not be valid
    JSON, so we do not have a programmatic way to determine the type of error.
    """
    endpoint = f"repos/{owner}/{name}/pulls"
    params: Dict[str, _Params] = {
        "base": base,
        "head": head,
        "body": body,
        "issue": issue,
        "draft": is_draft,
    }
    return await gh_cli.make_request(params, hostname=hostname, endpoint=endpoint)


async def update_pull_request(
    hostname: str,
    node_id: str,
    title: str,
    body: str,
    base: Optional[str],
) -> Result[str, str]:
    """Returns an "ID!" for the pull request, which should match the node_id
    that was passed in.

    If base is None, the base branch is left untouched. This is required for
    pull requests that are part of a native GitHub stack: GitHub rejects
    updatePullRequest mutations that include baseRefName for such pull
    requests, as the stack manages base branches itself.
    """
    params: Dict[str, _Params] = {
        "query": (
            query.GRAPHQL_UPDATE_PULL_REQUEST
            if base is not None
            else query.GRAPHQL_UPDATE_PULL_REQUEST_NO_BASE
        ),
        "pullRequestId": node_id,
        "title": title,
        "body": body,
    }
    if base is not None:
        params["base"] = base
    result = await gh_cli.make_request(params, hostname=hostname)
    if result.is_err():
        return Err(result.unwrap_err())
    else:
        return Ok(result.unwrap()["data"]["updatePullRequest"]["pullRequest"]["id"])


async def create_branch(
    *, hostname: str, repo_id: str, branch_name: str, oid: str
) -> Result[str, str]:
    """Attempts to create the branch. If successful, returns the ID of the newly
    created Ref.
    """
    params: Dict[str, _Params] = {
        "query": query.GRAPHQL_CREATE_BRANCH,
        "repositoryId": repo_id,
        "name": f"refs/heads/{branch_name}",
        "oid": oid,
    }
    result = await gh_cli.make_request(params, hostname=hostname)
    if result.is_err():
        return Err(result.unwrap_err())
    else:
        return Ok(result.unwrap()["data"]["createRef"]["ref"]["id"])


async def merge_into_branch(
    *, hostname: str, repo_id: str, oid_to_merge: str, branch_name: str
) -> Result[str, str]:
    """Takes the hash, oid_to_merge, and merges it into the specified branch_name.

    - base must be a branch name
    - oid_to_merge is the head to merge into base: can be a branch name or an oid
    """
    params: Dict[str, _Params] = {
        "query": query.GRAPHQL_MERGE_BRANCH,
        "repositoryId": repo_id,
        "base": branch_name,
        "head": oid_to_merge,
    }
    result = await gh_cli.make_request(params, hostname=hostname)
    if result.is_err():
        return Err(result.unwrap_err())
    else:
        return Ok(result.unwrap()["data"]["mergeBranch"]["mergeCommit"]["oid"])


async def get_username(hostname: str) -> Result[str, str]:
    """Returns the username associated with the auth token. Note that it is
    slightly faster to call graphql.try_parse_oath_token_from_hosts_yml() and
    read the value from hosts.yml.
    """
    params: Dict[str, _Params] = {
        "query": query.GRAPHQL_GET_LOGIN,
    }
    result = await gh_cli.make_request(params, hostname=hostname)
    if result.is_err():
        return Err(result.unwrap_err())
    else:
        return Ok(result.unwrap()["data"]["viewer"]["login"])


# Native GitHub "pull request stack" REST endpoints. The stacks API is in
# public preview and requires an explicit API version header:
# https://docs.github.com/en/rest/pulls/stacks
_STACKS_API_HEADERS = {"X-GitHub-Api-Version": "2026-03-10"}


@dataclass
class StackDetails:
    """A native GitHub pull request stack.

    https://docs.github.com/en/rest/pulls/stacks
    """

    # Number that identifies the stack within the repo. Note that GitHub
    # allocates stack numbers and pull request/issue numbers from disjoint
    # ranges, so a stack number never collides with a pull request number.
    number: int
    # URL for the stack.
    url: str
    # True if the stack is still open.
    is_open: bool
    # Numbers of the *open* pull requests in the stack, ordered from the
    # bottom of the stack (closest to the trunk) to the top. Merged and
    # closed pull requests are excluded.
    pull_requests: List[int]


def _parse_stack_from_dict(stack_obj: JsonDict) -> StackDetails:
    """Parses a "Pull Request Stack" object from the REST API.

    Note that merged (and otherwise closed) pull requests are excluded from
    `pull_requests`:

    >>> _parse_stack_from_dict({
    ...     "id": 1,
    ...     "number": 7,
    ...     "node_id": "PRS_1",
    ...     "url": "https://api.github.com/repos/facebook/sapling/stacks/7",
    ...     "open": True,
    ...     "base": {"ref": "main"},
    ...     "created_at": "2026-07-30T00:00:00Z",
    ...     "pull_requests": [
    ...         {"number": 101, "state": "closed",
    ...          "merged_at": "2026-07-30T01:00:00Z", "draft": False,
    ...          "head": {"ref": "pr101", "sha": "0" * 40}},
    ...         {"number": 102, "state": "open", "merged_at": None,
    ...          "draft": False, "head": {"ref": "pr102", "sha": "1" * 40}},
    ...         {"number": 103, "state": "open", "merged_at": None,
    ...          "draft": True, "head": {"ref": "pr103", "sha": "2" * 40}},
    ...     ],
    ... })
    StackDetails(number=7, url='https://api.github.com/repos/facebook/sapling/stacks/7', is_open=True, pull_requests=[102, 103])
    """
    return StackDetails(
        number=stack_obj["number"],
        url=stack_obj["url"],
        is_open=stack_obj["open"],
        pull_requests=[
            pr["number"] for pr in stack_obj["pull_requests"] if pr["state"] == "open"
        ],
    )


async def get_stack_for_pull_request(
    hostname: str, owner: str, name: str, number: int
) -> Result[Optional[StackDetails], str]:
    """Returns the stack containing the specified pull request, or None if the
    pull request is not part of a stack.
    """
    endpoint = f"repos/{owner}/{name}/stacks?pull_request={number}"
    result = await gh_cli.make_request(
        {}, hostname=hostname, endpoint=endpoint, headers=_STACKS_API_HEADERS
    )
    if result.is_err():
        return Err(result.unwrap_err())

    # The response is a JSON array of stacks. Because a pull request can be in
    # at most one stack, the `pull_request` filter yields at most one entry.
    stacks = result.unwrap()
    if not stacks:
        return Ok(None)
    return Ok(_parse_stack_from_dict(stacks[0]))


async def create_stack(
    hostname: str, owner: str, name: str, pr_numbers: List[int]
) -> Result[StackDetails, str]:
    """Creates a native GitHub stack from the specified pull requests.

    `pr_numbers` must be ordered from the bottom of the stack to the top: the
    bottom pull request's base must be the trunk, and each subsequent pull
    request's base branch must match the head branch of the one below it. The
    caller is responsible for having set up the base branches accordingly.
    """
    endpoint = f"repos/{owner}/{name}/stacks"
    params: Dict[str, ParamValue] = {"pull_requests": pr_numbers}
    result = await gh_cli.make_request(
        params,
        hostname=hostname,
        endpoint=endpoint,
        method="POST",
        headers=_STACKS_API_HEADERS,
    )
    if result.is_err():
        return Err(result.unwrap_err())
    return Ok(_parse_stack_from_dict(result.unwrap()))


async def add_prs_to_stack(
    hostname: str, owner: str, name: str, stack_number: int, pr_numbers: List[int]
) -> Result[StackDetails, str]:
    """Appends pull requests onto the top of an existing stack.

    `pr_numbers` must contain only the pull requests to add, ordered from the
    current top of the stack upward: the first one's base branch must match
    the head branch of the stack's current top pull request.
    """
    endpoint = f"repos/{owner}/{name}/stacks/{stack_number}/add"
    params: Dict[str, ParamValue] = {"pull_requests": pr_numbers}
    result = await gh_cli.make_request(
        params,
        hostname=hostname,
        endpoint=endpoint,
        method="POST",
        headers=_STACKS_API_HEADERS,
    )
    if result.is_err():
        return Err(result.unwrap_err())
    return Ok(_parse_stack_from_dict(result.unwrap()))


async def unstack(
    hostname: str, owner: str, name: str, stack_number: int
) -> Result[Optional[StackDetails], str]:
    """Removes the unmerged pull requests from a stack.

    Pull requests that cannot be unstacked (e.g., merged or queued for merge)
    are left in place. Returns the updated stack if pull requests remain in
    it; returns None if the stack was dissolved entirely (HTTP 204).
    """
    endpoint = f"repos/{owner}/{name}/stacks/{stack_number}/unstack"
    result = await gh_cli.make_request(
        {},
        hostname=hostname,
        endpoint=endpoint,
        method="POST",
        headers=_STACKS_API_HEADERS,
    )
    if result.is_err():
        return Err(result.unwrap_err())
    data = result.unwrap()
    if not data:
        return Ok(None)
    return Ok(_parse_stack_from_dict(data))
