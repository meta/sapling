# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import re
from typing import List, Tuple, Union

from sapling import error
from sapling.i18n import _

from .gh_submit import Repository

_HORIZONTAL_RULE = "---"
_SAPLING_FOOTER_MARKER = "[//]: # (BEGIN SAPLING FOOTER)"
DEFAULT_REVIEW_URL_TEMPLATE = "https://reviewstack.dev/{owner}/{repo}/pull/{number}"
DEFAULT_REVIEW_TOOL_NAME = "ReviewStack"


def create_pull_request_title_and_body(
    commit_msg_or_title_body: Union[str, Tuple[str, str]],
    pr_numbers_and_num_commits: List[Tuple[int, int]],
    pr_numbers_index: int,
    repository: Repository,
    reviewstack: bool = True,
    review_url_template: str = DEFAULT_REVIEW_URL_TEMPLATE,
    review_tool_name: str = DEFAULT_REVIEW_TOOL_NAME,
) -> Tuple[str, str]:
    r"""Returns (title, body) for the pull request.

    >>> commit_msg = 'The original commit message.\nSecond line of message.'
    >>> pr_numbers_and_num_commits = [(1, 1), (2, 2), (42, 1), (4, 1)]
    >>> pr_numbers_index = 2
    >>> upstream_repo = Repository(hostname="github.com", id="abcd=", owner="facebook", name="sapling", default_branch="main", is_fork=False)
    >>> contributor_repo = Repository(hostname="github.com", id="efgh=", owner="keith", name="sapling", default_branch="main", is_fork=True, upstream=upstream_repo)
    >>> title, body = create_pull_request_title_and_body(
    ...     commit_msg,
    ...     pr_numbers_and_num_commits,
    ...     pr_numbers_index,
    ...     contributor_repo,
    ... )
    >>> print(title)
    The original commit message.
    >>> reviewstack_url = "https://reviewstack.dev/facebook/sapling/pull/42"
    >>> print(body.replace(reviewstack_url, "{reviewstack_url}"))
    Second line of message.
    <BLANKLINE>
    ---
    [//]: # (BEGIN SAPLING FOOTER)
    Stack created with [Sapling](https://sapling-scm.com). Best reviewed with [ReviewStack]({reviewstack_url}).
    * #1
    * #2 (2 commits)
    * __->__ #42
    * #4

    Add trailing whitespace to commit_msg and ensure it is preserved.
    >>> commit_msg += '\n\n'
    >>> title, body = create_pull_request_title_and_body(
    ...     commit_msg,
    ...     pr_numbers_and_num_commits,
    ...     pr_numbers_index,
    ...     contributor_repo,
    ... )
    >>> print(title)
    The original commit message.
    >>> print(body.replace(reviewstack_url, "{reviewstack_url}"))
    Second line of message.
    <BLANKLINE>
    <BLANKLINE>
    ---
    [//]: # (BEGIN SAPLING FOOTER)
    Stack created with [Sapling](https://sapling-scm.com). Best reviewed with [ReviewStack]({reviewstack_url}).
    * #1
    * #2 (2 commits)
    * __->__ #42
    * #4

    Disable reviewstack message:
    >>> title, body = create_pull_request_title_and_body(commit_msg, pr_numbers_and_num_commits,
    ...     pr_numbers_index, contributor_repo, reviewstack=False)
    >>> print(title)
    The original commit message.
    >>> print(body)
    Second line of message.
    <BLANKLINE>
    <BLANKLINE>
    ---
    [//]: # (BEGIN SAPLING FOOTER)
    * #1
    * #2 (2 commits)
    * __->__ #42
    * #4

    Customize the review link, e.g. for a self-hosted review tool:
    >>> title, body = create_pull_request_title_and_body(
    ...     commit_msg,
    ...     pr_numbers_and_num_commits,
    ...     pr_numbers_index,
    ...     contributor_repo,
    ...     review_url_template="https://review.example.com/{owner}/{repo}/{number}",
    ...     review_tool_name="MyReview",
    ... )
    >>> print(body)
    Second line of message.
    <BLANKLINE>
    <BLANKLINE>
    ---
    [//]: # (BEGIN SAPLING FOOTER)
    Stack created with [Sapling](https://sapling-scm.com). Best reviewed with [MyReview](https://review.example.com/facebook/sapling/42).
    * #1
    * #2 (2 commits)
    * __->__ #42
    * #4

    A customized footer still parses as stack information:
    >>> parse_stack_information(body)
    [(False, 1), (False, 2), (True, 42), (False, 4)]

    Customizing only the tool name keeps the default URL:
    >>> title, body = create_pull_request_title_and_body(commit_msg, pr_numbers_and_num_commits,
    ...     pr_numbers_index, contributor_repo, review_tool_name="MyReview")
    >>> print(body.replace(reviewstack_url, "{reviewstack_url}"))
    Second line of message.
    <BLANKLINE>
    <BLANKLINE>
    ---
    [//]: # (BEGIN SAPLING FOOTER)
    Stack created with [Sapling](https://sapling-scm.com). Best reviewed with [MyReview]({reviewstack_url}).
    * #1
    * #2 (2 commits)
    * __->__ #42
    * #4

    The templates are ignored when the review link is disabled entirely:
    >>> title, body = create_pull_request_title_and_body(commit_msg, pr_numbers_and_num_commits,
    ...     pr_numbers_index, contributor_repo, reviewstack=False,
    ...     review_url_template="https://review.example.com/{owner}/{repo}/{number}",
    ...     review_tool_name="MyReview")
    >>> print(body)
    Second line of message.
    <BLANKLINE>
    <BLANKLINE>
    ---
    [//]: # (BEGIN SAPLING FOOTER)
    * #1
    * #2 (2 commits)
    * __->__ #42
    * #4

    An invalid URL template aborts with a message naming the config:
    >>> create_pull_request_title_and_body(commit_msg, pr_numbers_and_num_commits,
    ...     pr_numbers_index, contributor_repo,
    ...     review_url_template="https://review.example.com/{bogus}")
    Traceback (most recent call last):
     ...
    sapling.error.Abort: invalid github.pull-request-review-url-template 'https://review.example.com/{bogus}': 'bogus'

    Single commit stack:
    >>> title, body = create_pull_request_title_and_body("Foo", [(1, 1)], 0, contributor_repo)
    >>> print(title)
    Foo
    >>> print(body.replace(reviewstack_url, "{reviewstack_url}"))
    <BLANKLINE>
    >>> title, body = create_pull_request_title_and_body(("Foo", "Bar"), [(1, 1)], 0, contributor_repo)
    >>> print(title)
    Foo
    >>> print(body.replace(reviewstack_url, "{reviewstack_url}"))
    Bar
    """
    owner, name = repository.get_upstream_owner_and_name()
    pr = pr_numbers_and_num_commits[pr_numbers_index][0]

    try:
        title, body = commit_msg_or_title_body
    except ValueError:
        title, body = title_and_body(commit_msg_or_title_body)

    body = _strip_stack_information(body)
    extra = []
    if len(pr_numbers_and_num_commits) > 1:
        if reviewstack:
            # Pull requests live in the upstream repository, so - like `owner`
            # and `name` above (see get_upstream_owner_and_name()) - the
            # {hostname} placeholder expands from the upstream when submitting
            # from a fork. For non-fork clones, repository.hostname is the
            # same host.
            hostname = (
                repository.upstream.hostname
                if repository.upstream
                else repository.hostname
            )
            review_url = _format_review_url(
                review_url_template,
                owner=owner,
                repo=name,
                number=pr,
                hostname=hostname,
            )
            review_stack_message = f"Stack created with [Sapling](https://sapling-scm.com). Best reviewed with [{review_tool_name}]({review_url})."
            extra.append(review_stack_message)
        bulleted_list = "\n".join(
            _format_stack_entry(pr_number, index, pr_numbers_index, num_commits)
            for index, (pr_number, num_commits) in enumerate(pr_numbers_and_num_commits)
        )
        extra.append(bulleted_list)
    if extra:
        if body and not body.endswith("\n"):
            body += "\n"
        if body:
            body += "\n"
        body += "\n".join([_HORIZONTAL_RULE, _SAPLING_FOOTER_MARKER] + extra)
    return title, body


def _format_review_url(
    template: str, *, owner: str, repo: str, number: int, hostname: str
) -> str:
    r"""Expands the github.pull-request-review-url-template config value.

    >>> _format_review_url(DEFAULT_REVIEW_URL_TEMPLATE,
    ...     owner="facebook", repo="sapling", number=42, hostname="github.com")
    'https://reviewstack.dev/facebook/sapling/pull/42'
    >>> _format_review_url("https://{hostname}/{owner}/{repo}/reviews/{number}",
    ...     owner="facebook", repo="sapling", number=42, hostname="github.example.com")
    'https://github.example.com/facebook/sapling/reviews/42'
    """
    try:
        return template.format(owner=owner, repo=repo, number=number, hostname=hostname)
    except (IndexError, KeyError, ValueError) as e:
        raise error.Abort(
            _("invalid github.pull-request-review-url-template %r: %s") % (template, e)
        )


_STACK_ENTRY = re.compile(r"^\* (__->__ )?#([1-9]\d*).*$")

# Pair where the first value is True if this entry was noted as the "current"
# one with the "__->__" marker; otherwise, False.
# The second value is the pull request number for this entry.
_StackEntry = Tuple[bool, int]


def parse_stack_information(body: str) -> List[_StackEntry]:
    r"""
    With sapling stack footer marker:
    >>> reviewstack_url = "https://reviewstack.dev/facebook/sapling/pull/42"
    >>> body = (
    ...     'The original commit message.\n' +
    ...     'Second line of message.\n' +
    ...     '---\n' +
    ...     '[//]: # (BEGIN SAPLING FOOTER)\n' +
    ...     'Stack created with [Sapling](https://sapling-scm.com). ' +
    ...     f'Best reviewed with [ReviewStack]({reviewstack_url}).\n' +
    ...     '* #1\n' +
    ...     '* #2 (2 commits)\n' +
    ...     '* __->__ #42\n' +
    ...     '* #4\n')
    >>> parse_stack_information(body)
    [(False, 1), (False, 2), (True, 42), (False, 4)]

    Without sapling stack footer marker (legacy):
    >>> reviewstack_url = "https://reviewstack.dev/facebook/sapling/pull/42"
    >>> body = (
    ...     'The original commit message.\n' +
    ...     'Second line of message.\n' +
    ...     '---\n' +
    ...     'Stack created with [Sapling](https://sapling-scm.com). ' +
    ...     f'Best reviewed with [ReviewStack]({reviewstack_url}).\n' +
    ...     '* #1\n' +
    ...     '* #2 (2 commits)\n' +
    ...     '* __->__ #42\n' +
    ...     '* #4\n')
    >>> parse_stack_information(body)
    [(False, 1), (False, 2), (True, 42), (False, 4)]

    Free-text introduction lines between the modern marker and the bullet
    list are tolerated (e.g. customized prose, or the footer was edited by
    hand on github.com):
    >>> body = (
    ...     'The original commit message.\n' +
    ...     '---\n' +
    ...     '[//]: # (BEGIN SAPLING FOOTER)\n' +
    ...     'This stack is best reviewed commit by commit.\n' +
    ...     '\n' +
    ...     '* #1\n' +
    ...     '* __->__ #2\n')
    >>> parse_stack_information(body)
    [(False, 1), (True, 2)]

    Text after the bullet list still ends the list:
    >>> body = (
    ...     '[//]: # (BEGIN SAPLING FOOTER)\n' +
    ...     'Introduction.\n' +
    ...     '* #1\n' +
    ...     'Not a stack entry.\n' +
    ...     '* #2\n')
    >>> parse_stack_information(body)
    [(False, 1)]

    Without the modern marker (legacy bodies), free text still ends the
    search so that detection stays conservative:
    >>> body = (
    ...     'Stack created with [Sapling](https://sapling-scm.com).\n' +
    ...     'Some other prose.\n' +
    ...     '* #1\n')
    >>> parse_stack_information(body)
    []
    """
    in_stack_list = False
    saw_modern_marker = False
    seen_stack_entry = False
    stack_entries = []
    for line in body.splitlines():
        if _line_has_stack_list_marker(line):
            in_stack_list = True
            saw_modern_marker = saw_modern_marker or line == _SAPLING_FOOTER_MARKER
        elif in_stack_list:
            match = _STACK_ENTRY.match(line)
            if match:
                seen_stack_entry = True
                arrow, number = match.groups()
                stack_entries.append((bool(arrow), int(number, 10)))
            elif saw_modern_marker and not seen_stack_entry:
                # Free-text introduction between the modern footer marker and
                # the bullet list (e.g. prose customized or edited by hand on
                # github.com); skip it.
                continue
            else:
                # This must be the end of the list.
                break
    return stack_entries


def _line_has_stack_list_marker(line: str) -> bool:
    # we're still looking at the "Stack created with [Sapling]" text for backward compatibility
    return line == _SAPLING_FOOTER_MARKER or line.startswith(
        "Stack created with [Sapling]"
    )


_SAPLING_FOOTER_WITH_HRULE = re.compile(
    re.escape(_HORIZONTAL_RULE) + r"\r?\n" + re.escape(_SAPLING_FOOTER_MARKER),
    re.MULTILINE,
)


def _strip_stack_information(body: str) -> str:
    r"""
    Footer marker joined with \n
    >>> body = (
    ...     'The original commit message.\n' +
    ...     'Second line of message.\n' +
    ...     '---\n' +
    ...     '[//]: # (BEGIN SAPLING FOOTER)\n' +
    ...     '* #1\n' +
    ...     '* #2 (2 commits)\n' +
    ...     '* __->__ #42\n' +
    ...     '* #4\n')
    >>> _strip_stack_information(body)
    'The original commit message.\nSecond line of message.\n'

    Footer marker joined with \r\n. If the user edits the pull request body
    on github.com, GitHub will rewrite the line endings to \r\n.
    >>> _strip_stack_information(body.replace('\n', '\r\n'))
    'The original commit message.\r\nSecond line of message.\r\n'

    If the footer marker appears multiple times in the body, everything will
    be stripped after the first occurrence.
    >>> _strip_stack_information(body + body)
    'The original commit message.\nSecond line of message.\n'
    """
    return re.split(_SAPLING_FOOTER_WITH_HRULE, body, maxsplit=1)[0]


def _format_stack_entry(
    pr_number: int,
    pr_number_index: int,
    current_pr_index: int,
    num_commits: int,
) -> str:
    line = (
        f"* #{pr_number}"
        if current_pr_index != pr_number_index
        else f"* __->__ #{pr_number}"
    )
    if num_commits > 1:
        line += f" ({num_commits} commits)"
    return line


_EOL_PATTERN = re.compile(r"\r?\n")
_MAX_FIRSTLINE_LEN = 120


def firstline(msg: str) -> str:
    r"""Returns the "first line" of a commit message to use for the title of a
    pull request.

    >>> firstline("foobar")
    'foobar'
    >>> firstline("foo\nbar")
    'foo'
    >>> firstline("foo\r\nbar")
    'foo'
    >>> firstline("x" * (_MAX_FIRSTLINE_LEN + 1)) == "x" * _MAX_FIRSTLINE_LEN
    True
    """
    match = _EOL_PATTERN.search(msg)
    end = match.start() if match else len(msg)
    end = min(end, _MAX_FIRSTLINE_LEN)
    return msg[:end]


def title_and_body(msg: str) -> Tuple[str, str]:
    r"""Returns the title and body of a commit message.

    >>> title_and_body("foobar")
    ('foobar', '')
    >>> title_and_body("foo\nbar")
    ('foo', 'bar')
    >>> title_and_body("foo\r\nbar")
    ('foo', 'bar')
    >>> title_and_body("x" * (_MAX_FIRSTLINE_LEN + 1)) == ("x" * _MAX_FIRSTLINE_LEN, "x")
    True
    """
    title = firstline(msg)
    rest = msg[len(title) :]
    if rest.startswith("\n"):
        body = rest[1:]
    elif rest.startswith("\r\n"):
        body = rest[2:]
    else:
        body = rest
    return (title, body)
