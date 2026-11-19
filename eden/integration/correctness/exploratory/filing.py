# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""File a campaign's findings as subtasks of the findings parent task.

`file_findings` reads the `findings.json` that `findings.write_findings`
wrote and decides, per product-actionable task key, what the parent task's
subtasks already say about it:

- NEW: no subtask carries the key. Filing replays one of its runs and
  creates a subtask only when the replay reproduces the key.
- DUPLICATE: some subtask carries it, open or closed. Filing comments on
  every open carrier, or on every closed one when none is open, at most once
  per subtask, key, and daemon source commit, and only on carriers the filing
  owner owns; another owner's carrier is named, not commented on. Nothing is
  reopened.
- SKIPPED: a replay that did not reproduce the key, and routed or
  unclassified findings; nothing is written for them.

A subtask carries a key when its description holds the key's block
(`render_key_block`): a fenced block whose first line is `KEY_BLOCK_HEADER`
and the key, and whose second line is the canonical JSON of the key's
error-set document; the digest of that document is the key. Subtasks
without a block are not findings and are ignored.

Nothing is read from or written to the tasks when a gate fails: the
findings must be judged by the current checker, and every check line
`findings` recorded (campaign health, daemon provenance on every seed, each
red control) must be a PASS. A dry run reads the tasks and writes only
`filing.json` beside `findings.json`. With `max_new`, a run that would create
more subtasks than that files nothing. A predecision task read failure is
recorded before any writes. A write that fails is recorded on its decision,
and the rest still run. Every task access goes through `Tasks`.
"""

from __future__ import annotations

import json
import re
import subprocess
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Protocol

from eden.integration.correctness.exploratory.core.checker import semantic_digest
from eden.integration.correctness.exploratory.core.identity import (
    error_set_document,
    IDENTITY_KEY_SCHEMA,
)
from eden.integration.correctness.exploratory.core.target import Target
from eden.integration.correctness.exploratory.findings import (
    current_identity,
    FINDINGS_NAME,
    read_findings,
    Replayed,
    Row,
)
from eden.integration.correctness.exploratory.schemas import (
    canonical_json,
    document_digest,
    SchemaError,
    strict_loads,
    write_document,
)

FINDINGS_PARENT = "T293423744"
DEFAULT_TAG = "edenfs-exploratory"
FILING_NAME = "filing.json"
FILING_SCHEMA_VERSION = 1
KEY_BLOCK_HEADER = "edenfs-exploratory task key"
SIGHTING_HEADER = "edenfs-exploratory sighting"
COMMENT_LIMIT = 500
READ_ATTEMPTS = 3
READ_TIMEOUT_SECONDS = 60
READ_RETRY_DELAY_SECONDS = 1
_READ_COMMANDS = (("tasks.task", "describe"), ("tasks.comment", "list"))

NEW = "NEW"
DUPLICATE = "DUPLICATE"
SKIPPED = "SKIPPED"

_FENCE: re.Pattern[str] = re.compile(r"^```[^\n]*\n(.*?)\n```[ \t]*$", re.M | re.S)


class TasksError(RuntimeError):
    """A task read or write failed; the message names the command and its error."""


def _transient_read_failure(completed: subprocess.CompletedProcess[str]) -> bool:
    """Recognize the failed CAT-mint receive timeout observed in the nightly job."""
    if completed.stdout.strip():
        return False
    stderr = completed.stderr.lower()
    if any(
        denied in stderr
        for denied in ("access denied", "permission denied", "metaclitrustexception")
    ):
        return False
    _prefix, mint, cause = stderr.rpartition("failed to mint delegated cat")
    return bool(mint) and all(
        marker in cause
        for marker in (
            "c++ runtime error during cat request: (recv_timeout)",
            "for tier 'token.service'",
            "ttransportexception: timed out",
            "classified as infrastructure/transient",
        )
    )


@dataclass(frozen=True)
class TaskRecord:
    """One subtask as filing reads it."""

    number: str
    description: str
    closed: bool
    owner: str


@dataclass(frozen=True)
class TaskDraft:
    """The subtask filing creates for one key."""

    title: str
    description: str
    owner: str
    parent: str
    tag: str


class Tasks(Protocol):
    """Every task read and write filing makes. Only `create` and `comment` write."""

    def subtasks(self, parent: str) -> list[str]:
        """Return every subtask number (`T<number>`) of the parent.

        `MetaTasks` relies on `tasks.task describe --columns=subtasks`
        returning the complete list, not a page of it.
        """
        ...

    def task(self, number: str) -> TaskRecord:
        """Return one task's description, whether it is closed, and its owner."""
        ...

    def comments(self, number: str) -> list[str]:
        """Return the texts of a task's newest `COMMENT_LIMIT` comments."""
        ...

    def create(self, draft: TaskDraft) -> str:
        """Create the subtask and return its number."""
        ...

    def comment(self, number: str, text: str) -> None:
        """Add one comment to a task."""
        ...


class MetaTasks:
    """`Tasks` through the `meta` CLI, with bounded retries of CAT-mint read timeouts.

    Writes are attempted once. With `crewmate` set, every attempt runs as
    that crewmate identity through the global `meta --act-as-crewmate` flag.
    """

    def __init__(self, crewmate: str | None = None) -> None:
        self._identity: list[str] = (
            [] if crewmate is None else ["--act-as-crewmate", crewmate]
        )

    def _call(self, args: list[str], stdin: str | None = None) -> Any:
        command = f"meta {' '.join(args[:2])}"
        read = tuple(args[:2]) in _READ_COMMANDS
        attempts = READ_ATTEMPTS if read else 1
        for attempt in range(attempts):
            try:
                completed = subprocess.run(
                    ["meta", *self._identity, *args, "--output=json"],
                    input=stdin,
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=READ_TIMEOUT_SECONDS if read else None,
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                raise TasksError(f"{command} failed: {error}") from error
            if completed.returncode == 0:
                try:
                    return json.loads(completed.stdout)
                except json.JSONDecodeError as error:
                    raise TasksError(f"{command} printed no JSON: {error}") from error
            if attempt + 1 == attempts or not _transient_read_failure(completed):
                raise TasksError(
                    f"{command} exited {completed.returncode}: "
                    f"{completed.stderr.strip()[-2000:]}"
                )
            time.sleep(READ_RETRY_DELAY_SECONDS)
        raise TasksError(f"{command} was not attempted")

    def subtasks(self, parent: str) -> list[str]:
        found = self._call(
            ["tasks.task", "describe", f"--task={parent}", "--columns=subtasks"]
        )
        value = found.get("subtasks") if type(found) is dict else None
        if type(value) is not str:
            raise TasksError(f"{parent} subtasks are {value!r}, not text")
        return re.findall(r"\bT\d+\b", value)

    def task(self, number: str) -> TaskRecord:
        found = self._call(
            [
                "tasks.task",
                "describe",
                f"--task={number}",
                "--columns=description,completed,owner",
            ]
        )
        completed = found.get("completed") if type(found) is dict else None
        if type(completed) is not str:
            raise TasksError(f"{number} completed is {completed!r}, not text")
        # `completed` is "N/A" while the task is open, else its close time.
        return TaskRecord(
            number,
            str(found.get("description") or ""),
            completed != "N/A",
            str(found.get("owner") or ""),
        )

    def comments(self, number: str) -> list[str]:
        found = self._call(
            [
                "tasks.comment",
                "list",
                f"--task={number}",
                f"--limit={COMMENT_LIMIT}",
                "--columns=text",
            ]
        )
        if type(found) is not list:
            raise TasksError(f"{number} comments are not a list")
        return [str(c.get("text") or "") for c in found if type(c) is dict]

    def create(self, draft: TaskDraft) -> str:
        found = self._call(
            [
                "tasks.task",
                "create",
                f"--title={draft.title}",
                "--description=file://-",
                f"--owner={draft.owner}",
                f"--parent={draft.parent}",
                f"--add-tag={draft.tag}",
            ],
            stdin=draft.description,
        )
        number = found.get("number") if type(found) is dict else None
        if type(number) is not str:
            raise TasksError(f"tasks.task create returned no number: {found!r:.500}")
        return number

    def comment(self, number: str, text: str) -> None:
        self._call(["tasks.comment", "create", f"--task={number}", f"--text={text}"])


def key_document(errors: Any) -> dict[str, Any]:
    """Return the error-set document of a findings entry's `errors`."""
    return {"schema": IDENTITY_KEY_SCHEMA, "errors": errors}


def render_key_block(document: Mapping[str, Any]) -> str:
    """Return the fenced block that makes a task description carry a key."""
    key = document_digest(document)
    text = canonical_json(document).decode("utf-8")
    return f"```\n{KEY_BLOCK_HEADER} {key}\n{text}\n```"


def _block_problem(lines: Sequence[str]) -> tuple[str | None, dict[str, Any]]:
    """Return why a key block's lines are not exactly a key and its document, or None."""
    if len(lines) != 2:
        return f"{len(lines)} lines, not the header and one JSON line", {}
    key = lines[0].removeprefix(KEY_BLOCK_HEADER + " ")
    try:
        document = strict_loads(lines[1])
    except SchemaError as error:
        return f"{key}: {error}", {}
    if type(document) is not dict or set(document) != {"schema", "errors"}:
        return f"{key}: not an error-set document", {}
    try:
        canonical = error_set_document(
            (rule, kind, tuple((role, value) for role, value in roles))
            for rule, kind, roles in document["errors"]
        )
    except (TypeError, ValueError) as error:
        return f"{key}: malformed errors ({error})", {}
    if canonical != document:
        return f"{key}: not the canonical {IDENTITY_KEY_SCHEMA} document", {}
    if document_digest(document) != key:
        return f"{key}: the document's digest is {document_digest(document)}", {}
    return None, document


def parse_key_blocks(description: str) -> tuple[dict[str, dict[str, Any]], list[str]]:
    """Return a description's key documents by key, and why any key block is malformed.

    Only fenced blocks whose first line starts with `KEY_BLOCK_HEADER` are
    read; trailing whitespace on a line is ignored.
    """
    keys: dict[str, dict[str, Any]] = {}
    problems: list[str] = []
    for match in _FENCE.finditer(description):
        lines = [line.rstrip() for line in match.group(1).splitlines()]
        if not lines or not lines[0].startswith(KEY_BLOCK_HEADER + " "):
            continue
        problem, document = _block_problem(lines)
        if problem is not None:
            problems.append(problem)
        else:
            keys[document_digest(document)] = document
    return keys, problems


@dataclass(frozen=True)
class Subtasks:
    """The parent's subtasks by the keys their descriptions carry, and every malformed block."""

    carriers: Mapping[str, tuple[TaskRecord, ...]]
    malformed: tuple[Mapping[str, str], ...]


def read_subtasks(tasks: Tasks, parent: str) -> Subtasks:
    """Read every subtask of `parent` and index it by the key blocks it carries."""
    carriers: dict[str, list[TaskRecord]] = {}
    malformed: list[dict[str, str]] = []
    for number in tasks.subtasks(parent):
        record = tasks.task(number)
        keys, problems = parse_key_blocks(record.description)
        malformed += [{"task": number, "problem": p} for p in problems]
        for key in keys:
            carriers.setdefault(key, []).append(record)
    return Subtasks(
        {key: tuple(found) for key, found in carriers.items()}, tuple(malformed)
    )


def gate_failures(document: Row, target: Target) -> list[str]:
    """Name every reason the findings may not be filed; empty when all gates pass."""
    failures = []
    if document["target"] != target.name:
        failures.append(f"target: findings are for {document['target']!r}")
    if document["identity_digest"] != current_identity(target):
        failures.append("identity: findings were keyed by another rule registry")
    if document["semantic_digest"] != semantic_digest(target):
        failures.append("semantic: findings were judged by another checker")
    checks = document["checks"]
    if type(checks) is not list or not checks:
        failures.append("checks: findings recorded none")
    else:
        failures += [
            f"check: {line}" for line in checks if not str(line).startswith("PASS ")
        ]
    return failures


@dataclass(frozen=True)
class Options:
    """Where and how to file; `dry_run` reads the tasks and writes only `filing.json`."""

    parent: str
    owner: str
    tag: str = DEFAULT_TAG
    artifacts_url: str | None = None
    daemon_package: str | None = None
    dry_run: bool = True
    skip_replay: bool = False
    max_new: int | None = None


@dataclass(frozen=True)
class Decision:
    """What filing does, or would do, for one key or one unkeyed finding.

    A NEW decision carries `draft`; a DUPLICATE names the subtasks it
    comments on in `matched` and those still without this sighting in
    `comment_on`. `filed` names the tasks written, and `error` why a write
    failed.
    """

    action: str
    task_key: str
    detail: str
    matched: tuple[str, ...] = ()
    replay_run: str | None = None
    draft: TaskDraft | None = None
    comment: str | None = None
    comment_on: tuple[str, ...] = ()
    filed: tuple[str, ...] = ()
    error: str | None = None

    def line(self, dry_run: bool) -> str:
        """Return the decision's one printed line."""
        if self.action == SKIPPED:
            return f"{SKIPPED}({self.detail}) {self.task_key}"
        matched = f" {', '.join(self.matched)}" if self.matched else ""
        return f"{self.action} {self.task_key}{matched}: {self._outcome(dry_run)}"

    def _outcome(self, dry_run: bool) -> str:
        if self.draft is not None:
            replay = (
                "skip the replay"
                if self.replay_run is None
                else f"replay {self.replay_run}"
            )
            if self.filed:
                return f"{self.detail}; replayed; created {self.filed[0]}"
            verb = "would " if dry_run else ""
            return (
                f"{self.detail}; {verb}{replay}; {verb}create a subtask of"
                f" {self.draft.parent} owned by {self.draft.owner} tagged"
                f" {self.draft.tag}: {self.draft.title!r}"
            )
        parts = [self.detail]
        if self.comment_on:
            verb = "commented on" if self.filed else "would comment on"
            parts.append(f"{verb} {', '.join(self.comment_on)}")
        done = [n for n in self.matched if n not in self.comment_on]
        if done:
            parts.append(
                f"already commented on {', '.join(done)} for this source commit"
            )
        return "; ".join(parts)

    def document(self) -> dict[str, Any]:
        """Return the decision as `filing.json` records it."""
        return {
            "action": self.action,
            "task_key": self.task_key,
            "detail": self.detail,
            "matched": list(self.matched),
            "replay_run": self.replay_run,
            "create": None
            if self.draft is None
            else {
                "title": self.draft.title,
                "description": self.draft.description,
                "owner": self.draft.owner,
                "parent": self.draft.parent,
                "tag": self.draft.tag,
            },
            "comment": self.comment,
            "comment_on": list(self.comment_on),
            "filed": list(self.filed),
            "error": self.error,
        }


@dataclass(frozen=True)
class _Context:
    findings_dir: Path
    campaign: str
    options: Options


def _daemon_text(run: Row, options: Options) -> str:
    daemon = run["daemon"] or {}
    package = f", package `{options.daemon_package}`" if options.daemon_package else ""
    return (
        f"daemon `{daemon.get('binary_sha256')}` built from"
        f" `{daemon.get('source_commit')}`{package}"
    )


def _draft(entry: Row, context: _Context) -> TaskDraft:
    """Return a key's subtask: title from its rules and kinds; key block, body, and evidence."""
    pairs = sorted({(rule, kind) for rule, kind, _roles in entry["errors"]})
    options = context.options
    head = [
        f"Filed by `explore file` from campaign `{context.campaign}`;"
        f" {_daemon_text(entry['runs'][0], options)}.",
        f"Artifacts: {options.artifacts_url or 'not supplied'}",
    ]
    body = (context.findings_dir / entry["body"]).read_text(encoding="utf-8")
    block = render_key_block(key_document(entry["errors"]))
    return TaskDraft(
        "[eden][exploratory] " + ", ".join(f"{r} ({k})" for r, k in pairs),
        "\n\n".join([*head, block, body]),
        options.owner,
        options.parent,
        options.tag,
    )


def _sighting_marker(key: str, source_commit: str) -> str:
    """Return the first line of a key's sighting comment for one daemon source commit."""
    return f"{SIGHTING_HEADER} {key} {source_commit}"


def _sighting(entry: Row, marker: str, context: _Context) -> str:
    """Return a duplicate key's sighting comment: the marker, campaign, daemon, and runs."""
    runs = "\n".join(
        f"- seed {run['seed']}: `{run['run_dir']}` (RunSpec `{run['runspec_digest']}`)"
        for run in entry["runs"]
    )
    return (
        f"{marker}\n\nSeen again in campaign `{context.campaign}`;"
        f" {_daemon_text(entry['runs'][0], context.options)}.\n\n{runs}\n\n"
        f"Artifacts: {context.options.artifacts_url or 'not supplied'}"
    )


def decide(entry: Row, subtasks: Subtasks, tasks: Tasks, context: _Context) -> Decision:
    """Classify one findings entry against the subtasks that carry its key."""
    key = entry["task_key"]
    if document_digest(key_document(entry["errors"])) != key:
        return Decision(SKIPPED, key, "errors do not digest to the key")
    first = entry["runs"][0]
    carriers = sorted({t.number: t for t in subtasks.carriers.get(key, ())}.items())
    if not carriers:
        replay = None if context.options.skip_replay else str(first["run_dir"])
        return Decision(
            NEW, key, "no subtask carries it", (), replay, _draft(entry, context)
        )
    source_commit = (first["daemon"] or {}).get("source_commit")
    if source_commit is None:
        return Decision(SKIPPED, key, "no daemon source commit to dedup sightings")
    opened = tuple(n for n, t in carriers if not t.closed)
    targets = opened or tuple(n for n, _t in carriers)
    owners = {n: t.owner for n, t in carriers}
    mine = tuple(n for n in targets if owners[n] == context.options.owner)
    marker = _sighting_marker(key, source_commit)
    pending = tuple(n for n in mine if not any(marker in c for c in tasks.comments(n)))
    state = ", ".join(f"{n} ({'closed' if t.closed else 'open'})" for n, t in carriers)
    others = ", ".join(f"{n} (owned by {owners[n]})" for n in targets if n not in mine)
    return Decision(
        DUPLICATE,
        key,
        f"carried by {state}" + (f"; not commented on {others}" if others else ""),
        mine,
        comment=_sighting(entry, marker, context) if pending else None,
        comment_on=pending,
    )


Replay = Callable[[Path, Path], Replayed]


def act(
    decision: Decision, tasks: Tasks, replay: Replay, findings_dir: Path
) -> Decision:
    """Carry out one decision; a NEW key is created only once its replay reproduces it.

    A failed task write or replay is recorded on the decision with whatever
    it already wrote.
    """
    key = decision.task_key
    filed: list[str] = []
    try:
        for number in decision.comment_on:
            tasks.comment(number, decision.comment or "")
            filed.append(number)
        if decision.draft is not None:
            if decision.replay_run is not None:
                directory = findings_dir / "replays" / key.removeprefix("sha256:")[:16]
                replayed = replay(Path(decision.replay_run), directory)
                if key not in replayed.original or key not in replayed.replayed:
                    detail = f"replay {directory} did not reproduce the key"
                    return Decision(SKIPPED, key, detail, (), decision.replay_run)
            filed.append(tasks.create(decision.draft))
    except (TasksError, OSError) as error:
        return replace(
            decision, filed=tuple(filed), error=f"{type(error).__name__}: {error}"
        )
    return replace(decision, filed=tuple(filed))


def _unkeyed(document: Row) -> list[Decision]:
    """Return a SKIPPED decision per routed digest and per unclassified rule and cause."""
    routed: dict[tuple[str, str], list[int]] = {}
    for entry in document["routed"]:
        routed.setdefault((entry["route"], entry["digest"]), []).append(entry["seed"])
    unclassified: dict[tuple[str, str], list[int]] = {}
    for entry in document["unclassified"]:
        unclassified.setdefault((entry["rule_id"], entry["cause"]), []).append(
            entry["seed"]
        )
    return [
        *(
            Decision(SKIPPED, digest, f"routed-{route}; seeds {seeds}")
            for (route, digest), seeds in sorted(routed.items())
        ),
        *(
            Decision(SKIPPED, rule, f"unclassified-{cause}; seeds {seeds}")
            for (rule, cause), seeds in sorted(unclassified.items())
        ),
    ]


def _write(
    findings_dir: Path,
    document: Row,
    options: Options,
    gates: Sequence[str],
    decisions: Sequence[Decision],
    malformed: Sequence[Mapping[str, str]],
) -> Path:
    path = findings_dir / FILING_NAME
    write_document(
        path,
        {
            "schema_version": FILING_SCHEMA_VERSION,
            "campaign_id": document["campaign_id"],
            "parent": options.parent,
            "dry_run": options.dry_run,
            "gates": list(gates),
            "decisions": [d.document() for d in decisions],
            "malformed_key_blocks": list(malformed),
        },
    )
    return path


def file_findings(
    findings_dir: Path,
    target: Target,
    tasks: Tasks,
    options: Options,
    replay: Replay,
) -> tuple[int, list[str]]:
    """Gate, classify, and (unless a dry run) file one campaign's findings.

    Returns the exit code and the printed lines; `filing.json` records the
    same. The code is 1 when a gate or predecision read failed (nothing was
    filed), or any decision's write failed.
    """
    document = read_findings(findings_dir / FINDINGS_NAME)
    gates = gate_failures(document, target)
    if gates:
        path = _write(findings_dir, document, options, gates, (), ())
        return 1, [
            *(f"GATE FAIL {g}" for g in gates),
            f"filing: nothing filed; wrote {path}",
        ]
    _write(findings_dir, document, options, ["filing: not completed"], (), ())
    try:
        subtasks = read_subtasks(tasks, options.parent)
        context = _Context(findings_dir, document["campaign_id"], options)
        decisions = [
            decide(entry, subtasks, tasks, context) for entry in document["findings"]
        ]
    except (TasksError, OSError) as error:
        failure = f"predecision: {type(error).__name__}: {error}"
        path = _write(findings_dir, document, options, [failure], (), ())
        return 1, [f"GATE FAIL {failure}", f"filing: nothing filed; wrote {path}"]
    new = sum(d.action == NEW for d in decisions)
    if not options.dry_run and options.max_new is not None and new > options.max_new:
        cap = f"max-new: {new} new keys, more than {options.max_new}"
        decisions += _unkeyed(document)
        path = _write(
            findings_dir, document, options, [cap], decisions, subtasks.malformed
        )
        return 1, [
            f"GATE FAIL {cap}",
            *(d.line(True) for d in decisions),
            f"filing: nothing filed; wrote {path}",
        ]
    if not options.dry_run:
        decisions = [act(d, tasks, replay, findings_dir) for d in decisions]
    decisions += _unkeyed(document)
    path = _write(findings_dir, document, options, (), decisions, subtasks.malformed)
    lines = []
    for decision in decisions:
        lines.append(decision.line(options.dry_run))
        if decision.error is not None:
            lines.append(f"ERROR {decision.task_key}: {decision.error}")
    lines += [
        f"KEY BLOCK MALFORMED {m['task']}: {m['problem']}" for m in subtasks.malformed
    ]
    errors = sum(d.error is not None for d in decisions)
    mode = "dry run, nothing filed" if options.dry_run else f"filed, {errors} errors"
    lines.append(f"filing: {len(document['findings'])} keys, {mode}; wrote {path}")
    return int(errors > 0), lines
