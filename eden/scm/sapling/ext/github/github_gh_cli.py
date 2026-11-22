# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import asyncio
import itertools
import json
import os
from typing import Any, Dict, List, Optional, Union

from sapling import error
from sapling.i18n import _
from sapling.result import Err, Ok, Result


JsonDict = Dict[str, Any]

# Scalar value that can be passed as a field to `gh api`.
_ScalarParam = Union[str, int, bool]
# `gh api` also supports array fields via repeated `key[]=value` args.
ParamValue = Union[_ScalarParam, List[_ScalarParam]]


async def make_request(
    params: Dict[str, ParamValue],
    hostname: str,
    endpoint="graphql",
    method: Optional[str] = None,
    headers: Optional[Dict[str, str]] = None,
) -> Result[JsonDict, str]:
    """If successful, returns a Result whose value is parsed JSON returned by
    the request.
    """
    return await _make_request(params, hostname, endpoint, method, headers)


# Unexported extension/mock point.
async def _make_request(
    params: Dict[str, ParamValue],
    hostname: str,
    endpoint: str,
    method: Optional[str],
    headers: Optional[Dict[str, str]] = None,
) -> Result[JsonDict, str]:
    if method:
        endpoint_args = ["-X", method.upper(), endpoint]
    else:
        endpoint_args = [endpoint]
    header_args = list(
        itertools.chain(*[["-H", f"{k}: {v}"] for (k, v) in (headers or {}).items()])
    )
    args = (
        ["gh", "api", "--hostname", hostname]
        + header_args
        + endpoint_args
        + list(itertools.chain(*[_format_param(k, v) for (k, v) in params.items()]))
    )

    # https://cli.github.com/manual/gh_help_environment documents support for
    # CLICOLOR and CLICOLOR_FORCE. Note that a user unknowingly had
    # CLICOLOR_FORCE=1 set in a zsh script somewhere and got a very confusing
    # error as reported on https://github.com/facebook/sapling/issues/146
    # because the output of gh could not be parsed via json.loads(), so we
    # explicitly disable ANSI colors in our piped output.
    env = os.environ.copy()
    env["CLICOLOR_FORCE"] = "0"
    env["TCELL_MINIMIZE"] = "1"
    proc = await asyncio.create_subprocess_exec(
        *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=env
    )
    stdout, stderr = await proc.communicate()

    # If proc exits with a non-zero exit code, the stdout may still
    # be valid JSON, but we expect it to have an "errors" property defined.
    try:
        response = json.loads(stdout)
    except json.JSONDecodeError:
        response = None

    if proc.returncode == 0:
        if response is None:
            # Some REST endpoints return "204 No Content" on success (e.g.,
            # dissolving a pull request stack), in which case `gh api` prints
            # no JSON to parse.
            if not stdout.strip():
                return Ok({})
            return Err(f"could not parse JSON from response: {stdout.decode()}")
        assert "errors" not in response
        return Ok(response)
    elif response is not None:
        return Err(json.dumps(response, indent=1))
    elif b"gh auth login" in stderr:
        # The error message is likely referring to an authentication issue.
        raise error.Abort(_("Error calling the GitHub API:\n%s") % stderr.decode())
    else:
        return Err(
            f"exit({proc.returncode}) Failure running {' '.join(args)}\nstdout: {stdout.decode()}\nstderr: {stderr.decode()}\n"
        )


def _format_param(key: str, value: ParamValue) -> List[str]:
    r"""Formats a param as a list of arguments to pass to `gh api`.

    >>> _format_param("body", "hello")
    ['-f', 'body=hello']
    >>> _format_param("number", 42)
    ['-F', 'number=42']
    >>> _format_param("draft", True)
    ['-F', 'draft=true']

    Array values use the `gh api` repeated-field syntax, e.g.
    `-F "pull_requests[]=101" -F "pull_requests[]=102"`:

    >>> _format_param("pull_requests", [101, 102])
    ['-F', 'pull_requests[]=101', '-F', 'pull_requests[]=102']
    >>> _format_param("labels", ["bug", "help wanted"])
    ['-f', 'labels[]=bug', '-f', 'labels[]=help wanted']

    An empty array is passed explicitly as `key[]` without a value (per the
    `gh api` manual) rather than dropped: an empty array and a missing field
    can have different meanings to an endpoint:

    >>> _format_param("empty", [])
    ['-F', 'empty[]']
    """
    if isinstance(value, list):
        if not value:
            return ["-F", f"{key}[]"]
        return list(
            itertools.chain(*[_format_param(f"{key}[]", v) for v in value])
        )

    # In Python, bool is a subclass of int, so check it first.
    if isinstance(value, bool):
        opt = "-F"
        val = str(value).lower()
    elif isinstance(value, int):
        opt = "-F"
        val = value
    elif isinstance(value, str):
        opt = "-f"
        val = str(value)
    else:
        raise RuntimeError(f"unexpected param: {key}={value}")
    return [opt, f"{key}={val}"]
