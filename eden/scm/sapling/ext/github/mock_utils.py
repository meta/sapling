# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import hashlib
from abc import abstractmethod
from typing import Any, Callable, Dict, List, Optional, Union

from sapling import error
from sapling.ext.github.consts import query
from sapling.ext.github.gh_submit import PullRequestState
from sapling.ext.github.pull_request_body import title_and_body
from sapling.result import Ok, Result

from .consts import GITHUB_HOSTNAME
from .github_gh_cli import JsonDict

"""utils for mocking GitHub requests.

This is defined to be used with `extensions.wrapfunction` to replace the real
GitHub requests in tests.
"""

OWNER = "facebook"
OWNER_ID = "facebook_id"
REPO_NAME = "test_github_repo"
REPO_ID = "R_test_github_repo"
USER_NAME = "facebook_username"

ParamsType = Dict[str, Union[bool, int, str, List[bool], List[int], List[str]]]
MakeRequestType = Callable[
    [ParamsType, str, str, Optional[str], Optional[Dict[str, str]]],
    Result[JsonDict, str],
]
RunGitCommandType = Callable[[List[str], str], bytes]


# TODO:
# 1. verify the arguments of the git push commands
def mock_run_git_command(
    origin_run_git_command: RunGitCommandType,
    args: List[str],
    gitdir: str,
) -> bytes:
    """Wrapper function for `run_git_command`.

    For 'git push' operation, it just returns without sending a network request;
    otherwise call the real function.
    """
    if args and args[0] == "push":
        return b""
    return origin_run_git_command(args, gitdir)


# TODO:
# 1. a way to verify all of the expected mocks in MockGitHubServer were called.
#      * one way to do this is logging each call to a file
# 2. for multiple commands, we need to create a config extension for each command.
#    Should we maintain a state between commands?
# 3. improve error message, when a request_key not found in the server mock data,
#    try to find the closest key and show the diff.
class MockGitHubServer:
    """A mock GitHub server for replacing `github_gh_cli.make_request`.

    The internal `requests` is a mapping of request_keys and mock requests. .

    Example usage::

      github_server = MockGithubServer()
      github_server.expect_get_repository_request().and_respond()

      extensions.wrapfunction(
        github_gh_cli, "make_request", github_server.make_request
      )
    """

    def __init__(self, hostname: str = GITHUB_HOSTNAME):
        self.hostname: str = hostname
        self.requests: Dict[str, MockRequest] = {}
        self._consumed_keys: set = set()

    async def make_request(
        self,
        real_make_request: MakeRequestType,
        params: ParamsType,
        hostname: str,
        endpoint: str = "graphql",
        method: Optional[str] = None,
        headers: Optional[Dict[str, str]] = None,
    ) -> Result[JsonDict, str]:
        """Wrapper function for `github_gh_cli.make_request`.

        It reads mock data from `self.requests` instead of sending network requests.

        Note that `headers` is intentionally not part of the request key: the
        headers we send (e.g., X-GitHub-Api-Version) do not affect which mock
        response should be returned.
        """
        assert real_make_request.__name__ == "_make_request", (
            f"expected '_make_request', but got '{real_make_request.__name__}'"
        )

        key = create_request_key(params, hostname, endpoint, method)

        if key not in self.requests:
            raise MockRequestNotFound(key, self.requests)
        self._consumed_keys.add(key)
        return self.requests[key].get_response()

    def _add_request(self, request_key: str, request: "MockRequest") -> None:
        self.requests[request_key] = request

    def unconsumed_requests(self) -> List[str]:
        """Keys of expectations that were never requested."""
        return sorted(k for k in self.requests if k not in self._consumed_keys)

    def report_unconsumed(self, ui) -> None:
        """Prints a warning for every expectation that was never requested.

        Intended to be called after the command under test has finished (see
        wrap_with_consumption_check). Tests that use this produce no extra
        output when all expectations were consumed, so any warning makes the
        test fail: this catches code paths that silently stopped making a
        request the test author expected.
        """
        for key in self.unconsumed_requests():
            first_line = key.splitlines()[0]
            ui.status_err(f"warning, unconsumed mock request: {first_line}\n")


def wrap_with_consumption_check(server: "MockGitHubServer", module, funcname) -> None:
    """Wraps `module.funcname` (a command function taking `ui` as its first
    argument) so that unconsumed mock expectations are reported after the
    command finishes, even if it aborts.
    """
    from sapling import extensions

    def wrapped(orig, ui, *args, **kwargs):
        try:
            return orig(ui, *args, **kwargs)
        finally:
            server.report_unconsumed(ui)

    extensions.wrapfunction(module, funcname, wrapped)

    def expect_get_repository_request(
        self, owner: str = OWNER, name: str = REPO_NAME
    ) -> "GetRepositoryRequest":
        params: ParamsType = {
            "query": query.GRAPHQL_GET_REPOSITORY,
            "owner": owner,
            "name": name,
        }
        key = create_request_key(params, self.hostname)
        request = GetRepositoryRequest(key, owner, name)
        self._add_request(key, request)
        return request

    def expect_create_pr_placeholder_request(
        self, owner: str = OWNER, name: str = REPO_NAME
    ) -> "CreatePrPlaceholderRequest":
        endpoint = f"repos/{owner}/{name}/issues"
        params: ParamsType = {
            "title": "placeholder for pull request",
        }
        key = create_request_key(params, self.hostname, endpoint=endpoint)
        request = CreatePrPlaceholderRequest(key)
        self._add_request(key, request)
        return request

    def expect_guess_next_pull_request_number(
        self, owner: str = OWNER, name: str = REPO_NAME
    ) -> "GuessNextPrNumberRequest":
        params: ParamsType = {
            "query": query.GRAPHQL_GET_MAX_PR_ISSUE_NUMBER,
            "owner": owner,
            "name": name,
        }
        key = create_request_key(params, self.hostname)
        request = GuessNextPrNumberRequest(key)
        self._add_request(key, request)
        return request

    def expect_create_pr_request(
        self,
        body: str,
        title: str,
        head: str,
        is_draft: bool = False,
        base: str = "main",
        owner: str = OWNER,
        name: str = REPO_NAME,
        method: Optional[str] = None,
        **more_params,
    ) -> "CreatePrRequest":
        params: ParamsType = {
            "base": base,
            "head": head,
            "body": body,
            "title": title,
            "draft": is_draft,
            **more_params,
        }
        endpoint = f"repos/{owner}/{name}/pulls"
        key = create_request_key(
            params, self.hostname, endpoint=endpoint, method=method
        )
        request = CreatePrRequest(key, owner, name)
        self._add_request(key, request)
        return request

    def expect_create_pr_using_placeholder_request(
        self,
        body: str,
        issue: int,
        head: str = "",
        is_draft: bool = False,
        base: str = "main",
        owner: str = OWNER,
        name: str = REPO_NAME,
    ) -> "CreatePrUsingPlaceholderRequest":
        params: ParamsType = {
            "base": base,
            "head": head or f"pr{issue}",
            "body": body,
            "issue": issue,
            "draft": is_draft,
        }

        endpoint = f"repos/{owner}/{name}/pulls"
        key = create_request_key(params, self.hostname, endpoint=endpoint)
        request = CreatePrUsingPlaceholderRequest(key, owner, name, issue)
        self._add_request(key, request)
        return request

    def expect_request(
        self,
        params: ParamsType,
        response: JsonDict,
        endpoint: str = "graphql",
        method: Optional[str] = None,
    ):
        key = create_request_key(
            params, self.hostname, endpoint=endpoint, method=method
        )
        self._add_request(key, GenericRequest(response))

    def expect_pr_to_ref(
        self,
        pr_number: int,
        ref: str,
        owner: str = OWNER,
        name: str = REPO_NAME,
    ) -> None:
        self.expect_request(
            params={
                "query": query.GRAPHQL_PR_TO_REF,
                "owner": owner,
                "name": name,
                "number": pr_number,
            },
            response={
                "data": {
                    "repository": {
                        "pullRequest": {
                            "headRefName": ref,
                        },
                    },
                },
            },
        )

    def expect_ref_to_commit_and_tree(
        self,
        ref: str,
        commit: str,
        tree: str,
        repo_id: str = REPO_ID,
    ) -> None:
        self.expect_request(
            params={
                "query": query.GRAPHQL_REF_TO_COMMIT_AND_TREE,
                "repo_id": repo_id,
                "ref": ref,
            },
            response={
                "data": {
                    "node": {
                        "ref": {
                            "target": {
                                "oid": commit,
                                "tree": {
                                    "oid": tree,
                                },
                            },
                        },
                    },
                },
            },
        )

    def expect_get_pr_details_request(
        self,
        pr_number: int,
        owner: str = OWNER,
        name: str = REPO_NAME,
    ) -> "GetPrDetailsRequest":
        params: ParamsType = {
            "query": query.GRAPHQL_GET_PULL_REQUEST,
            "owner": owner,
            "name": name,
            "number": pr_number,
        }
        key = create_request_key(params, self.hostname)
        request = GetPrDetailsRequest(key, owner, name, pr_number)
        self._add_request(key, request)
        return request

    def expect_update_pr_request(
        self,
        pr_id: str,
        pr_number: int,
        commit_msg: str,
        base: str = "main",
        owner: str = OWNER,
        name: str = REPO_NAME,
        stack_pr_ids: Optional[List[int]] = None,
        *,
        review_url: str,
        review_tool: str,
    ) -> "UpdatePrRequest":
        if not stack_pr_ids:
            stack_pr_ids = [pr_number]
        stack_pr_ids = list(reversed(sorted(stack_pr_ids)))

        if len(stack_pr_ids) > 1:
            pr_list = [
                f"* __->__ #{n}" if n == pr_number else f"* #{n}" for n in stack_pr_ids
            ]
            commit_msg += (
                ("" if commit_msg.endswith("\n") else "\n") + "---\n"
                "[//]: # (BEGIN SAPLING FOOTER)\n"
                "Stack created with [Sapling](https://sapling-scm.com). Best reviewed"
                f" with [{review_tool}]({review_url}).\n" + "\n".join(pr_list)
            )

        title, body = title_and_body(commit_msg)
        params: ParamsType = {
            "query": query.GRAPHQL_UPDATE_PULL_REQUEST,
            "pullRequestId": pr_id,
            "title": title,
            "body": body,
            "base": base,
        }
        key = create_request_key(params, self.hostname)
        request = UpdatePrRequest(key, pr_id)
        self._add_request(key, request)
        return request

    def expect_get_username_request(
        self,
    ) -> "GetUsernameRequest":
        params: ParamsType = {"query": query.GRAPHQL_GET_LOGIN}
        key = create_request_key(params, self.hostname)
        request = GetUsernameRequest(key)
        self._add_request(key, request)
        return request

    def expect_merge_into_branch(
        self,
        head: str,
        username: str = USER_NAME,
        repo_id: str = REPO_ID,
        base: str = "",
    ) -> "MergeIntoBranchRequest":
        base = base or f"sapling-pr-archive-{username}"
        params: ParamsType = {
            "query": query.GRAPHQL_MERGE_BRANCH,
            "repositoryId": repo_id,
            "base": base,
            "head": head,
        }
        key = create_request_key(params, self.hostname)
        request = MergeIntoBranchRequest(key, head)
        self._add_request(key, request)
        return request


class MockRequest:
    @abstractmethod
    def get_response(self) -> Result[JsonDict, str]:
        pass


class GenericRequest(MockRequest):
    def __init__(self, response: JsonDict) -> None:
        self._response: JsonDict = response

    def get_response(self) -> Result[JsonDict, str]:
        return Ok(self._response)


class GetRepositoryRequest(MockRequest):
    def __init__(self, key: str, owner: str, name: str) -> None:
        self._key = key
        self._response: Optional[Result[JsonDict, str]] = None

        self._owner = owner
        self._name = name

    def and_respond(
        self,
        repo_id: str = REPO_ID,
        owner_id: str = OWNER_ID,
        is_fork: bool = False,
        default_branch_ref: str = "main",
        parent: Optional[Any] = None,
    ):
        data = {
            "data": {
                "repository": {
                    "id": repo_id,
                    "owner": {"id": owner_id, "login": self._owner},
                    "name": self._name,
                    "isFork": is_fork,
                    "defaultBranchRef": {"name": default_branch_ref},
                    "parent": parent,
                }
            }
        }
        self._response = Ok(data)

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class CreatePrPlaceholderRequest(MockRequest):
    def __init__(self, key: str) -> None:
        self._key = key

        self._current_number = 1
        self._num_times = 1

    def and_respond(self, start_number: int = 1, num_times: int = 1) -> None:
        self._current_number = start_number
        self._num_times = num_times

    def get_response(self) -> Result[JsonDict, str]:
        if self._num_times > 0:
            number = self._current_number
            self._current_number += 1
            self._num_times -= 1
            return Ok({"number": number})
        else:
            raise MockResponseRunout(self._key)


class GuessNextPrNumberRequest(MockRequest):
    def __init__(self, key: str) -> None:
        self._key = key
        self._response: Optional[Result[JsonDict, str]] = None

    def and_respond(self, latest_issue_num: int = 40, latest_pr_num: int = 41) -> None:
        data = {
            "data": {
                "repository": {
                    "issues": {"nodes": [{"number": latest_issue_num}]},
                    "pullRequests": {"nodes": [{"number": latest_pr_num}]},
                }
            }
        }
        self._response = Ok(data)

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class CreatePrRequest(MockRequest):
    def __init__(self, key: str, owner, name: str) -> None:
        self._key: str = key
        self._response: Optional[Result[JsonDict, str]] = None

        self._owner = owner
        self._name = name

    def and_respond(self, number: int) -> None:
        self._response = Ok(
            {
                "number": number,
                "html_url": f"https://github.com/{self._owner}/{self._name}/pull/{number}",
            }
        )

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class CreatePrUsingPlaceholderRequest(MockRequest):
    def __init__(self, key: str, owner, name: str, number: int) -> None:
        self._key: str = key
        self._response: Optional[Result[JsonDict, str]] = None

        self._owner = owner
        self._name = name
        self._number = number

    def and_respond(self):
        self._response = Ok(
            {
                "number": self._number,
                "html_url": f"https://github.com/{self._owner}/{self._name}/pull/{self._number}",
            }
        )

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class GetPrDetailsRequest(MockRequest):
    def __init__(self, key: str, owner: str, name: str, pr_number: int) -> None:
        self._key = key
        self._response: Optional[Result[JsonDict, str]] = None

        self._owner = owner
        self._name = name
        self._pr_number = pr_number

    def and_respond(
        self,
        pr_id: str,
        state: PullRequestState = PullRequestState.OPEN,
        head_ref_name: str = "",
        head_ref_oid: str = "",
        base_ref_name: str = "main",
        base_ref_oid: str = "",
        body: str = "",
        title: str = "",
    ):
        head_ref_name = head_ref_name or f"pr{self._pr_number}"
        head_ref_oid = head_ref_oid or gen_hash_hexdigest(pr_id)
        base_ref_oid = base_ref_oid or gen_hash_hexdigest(base_ref_name)
        data = {
            "data": {
                "repository": {
                    "pullRequest": {
                        "id": pr_id,
                        "url": f"https://github.com/{self._owner}/{self._name}/pull/{self._pr_number}",
                        "state": state.name,
                        "headRefOid": head_ref_oid,
                        "headRefName": head_ref_name,
                        "baseRefOid": base_ref_oid,
                        "baseRefName": base_ref_name,
                        "body": body,
                        "title": title,
                    }
                }
            }
        }
        self._response = Ok(data)

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class UpdatePrRequest(MockRequest):
    def __init__(self, key: str, pr_id: str) -> None:
        self._key = key
        self._response: Optional[Result[JsonDict, str]] = None

        self._pr_id = pr_id

    def and_respond(self):
        data = {"data": {"updatePullRequest": {"pullRequest": {"id": self._pr_id}}}}
        self._response = Ok(data)

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class GetUsernameRequest(MockRequest):
    def __init__(self, key: str) -> None:
        self._key = key
        self._response: Optional[Result[JsonDict, str]] = None

    def and_respond(self, username: str = USER_NAME):
        data = {"data": {"viewer": {"login": f"{username}"}}}
        self._response = Ok(data)

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class MergeIntoBranchRequest(MockRequest):
    def __init__(self, key: str, head: str) -> None:
        self._key = key
        self._response: Optional[Result[JsonDict, str]] = None

        self._head = head

    def and_respond(self, merge_commit_oid: str = ""):
        merge_commit_oid = merge_commit_oid or gen_hash_hexdigest(self._head)
        data = {"data": {"mergeBranch": {"mergeCommit": {"oid": merge_commit_oid}}}}
        self._response = Ok(data)

    def get_response(self) -> Result[JsonDict, str]:
        if self._response is None:
            raise MockResponseNotSet(self._key)
        return self._response


class MockRequestNotFound(error.Abort):
    def __init__(self, key: str, requests: Dict[str, MockRequest]) -> None:
        import textwrap

        from sapling import mdiff

        # Try to find a similar key. The diff should be within 30 lines.
        best_diff = None
        best_diff_lines = 30
        for existing_key in requests.keys():
            diff_text = b"".join(
                t
                for _b, ts in mdiff.unidiff(
                    existing_key.encode(), "", key.encode(), "", "existing", "got"
                )[1]
                for t in ts
            ).decode()
            diff_lines = diff_text.count("\n")
            if diff_lines < best_diff_lines:
                best_diff = diff_text
                best_diff_lines = diff_lines
        if best_diff:
            msg = f"Mock key has changed:\n{textwrap.indent(best_diff, '| ')}"
        else:
            key_list = "\n\n".join(textwrap.indent(k, "| ") for k in requests.keys())
            indented_key = textwrap.indent(key, "| ")
            msg = f"key not found:\n{indented_key}\nAvailable keys:\n{key_list}"
        super().__init__(msg)


class MockResponseNotSet(Exception):
    pass


class MockResponseRunout(Exception):
    pass


def create_request_key(
    params: ParamsType,
    hostname: str = GITHUB_HOSTNAME,
    endpoint: str = "graphql",
    method: Optional[str] = None,
) -> str:
    """Create a string key from the input of `make_request` function.

    This will be used to verify the input and find corresponding output.
    """
    s = ",".join(f"{k}={v}" for k, v in sorted(params.items()))
    v = f"{hostname}|{endpoint}|{method}|{s}"
    return v


def gen_hash_hexdigest(s: str) -> str:
    """generate sha1 digit hex string for input `s`"""
    return hashlib.sha1(s.encode()).hexdigest()
