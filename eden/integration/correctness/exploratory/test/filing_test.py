# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Filing findings as subtasks, against an in-memory `Tasks`.

Gates file nothing and read no task; only product keys are filed; a new key
is created only after its replay reproduces it; any carrier, open or
closed, gets a sighting comment instead, once per daemon source commit;
subtasks without a key block are not findings; a failed write is recorded
and the rest still run; a dry run never writes; and `MetaTasks` passes the
crewmate flag on every call.
"""

from __future__ import annotations

import io
import json
import subprocess
import tempfile
import unittest
from collections.abc import Sequence
from contextlib import redirect_stdout
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any
from unittest import mock

from eden.integration.correctness.exploratory import filing, findings
from eden.integration.correctness.exploratory.cli import _main
from eden.integration.correctness.exploratory.core.checker import semantic_digest
from eden.integration.correctness.exploratory.fake.target import FAKE
from eden.integration.correctness.exploratory.filing import (
    Options,
    TaskDraft,
    TaskRecord,
    TasksError,
)
from eden.integration.correctness.exploratory.findings import Replayed
from eden.integration.correctness.exploratory.schemas import (
    document_digest,
    FINDINGS_SCHEMA_VERSION,
)
from eden.integration.correctness.exploratory.test.cli_fixtures import fake_campaign

PARENT = "T100"
COMMIT = "c" * 40
TORN = [["bytes.torn-read", "torn-read", [["channel", "fuse"]]]]
THRIFT = [["bytes.torn-read", "torn-read", [["channel", "thrift"]]]]
TORN_KEY = document_digest(filing.key_document(TORN))
THRIFT_KEY = document_digest(filing.key_document(THRIFT))
PASSING = ["PASS health harness-failures", "PASS provenance on every seed"]
OPTIONS = Options(parent=PARENT, owner="owner", artifacts_url="https://example/a")
FILE = replace(OPTIONS, dry_run=False)


@dataclass
class FakeTasks:
    """In-memory tasks; every call is recorded, and writes fail when `read_only`."""

    records: dict[str, TaskRecord] = field(default_factory=dict)
    notes: dict[str, list[str]] = field(default_factory=dict)
    read_only: bool = False
    failing: set[str] = field(default_factory=set)
    calls: list[tuple[str, str]] = field(default_factory=list)
    created: list[TaskDraft] = field(default_factory=list)

    def add(
        self, number: str, description: str, closed: bool = False, owner: str = "owner"
    ) -> None:
        self.records[number] = TaskRecord(number, description, closed, owner)

    def subtasks(self, parent: str) -> list[str]:
        self.calls.append(("subtasks", parent))
        return list(self.records)

    def task(self, number: str) -> TaskRecord:
        self.calls.append(("task", number))
        return self.records[number]

    def comments(self, number: str) -> list[str]:
        self.calls.append(("comments", number))
        return list(reversed(self.notes.get(number, [])))

    def create(self, draft: TaskDraft) -> str:
        self.calls.append(("create", draft.title))
        assert not self.read_only, "a dry run created a task"
        if "create" in self.failing:
            raise TasksError("meta tasks.task create exited 1: denied")
        self.created.append(draft)
        return f"T{900 + len(self.created)}"

    def comment(self, number: str, text: str) -> None:
        self.calls.append(("comment", number))
        assert not self.read_only, "a dry run commented"
        if number in self.failing:
            raise TasksError("meta tasks.comment create exited 1: denied")
        self.notes.setdefault(number, []).append(text)

    def writes(self) -> list[tuple[str, str]]:
        return [call for call in self.calls if call[0] in ("create", "comment")]


def entry(errors: list[Any] = TORN, commit: str = COMMIT) -> dict[str, Any]:
    """Return one findings entry of `errors`, seen in seeds 3 and 5 on daemon `commit`."""
    return {
        "task_key": document_digest(filing.key_document(errors)),
        "errors": errors,
        "rules": sorted({rule for rule, _kind, _roles in errors}),
        "related": [],
        "runs": [
            {
                "seed": seed,
                "run_dir": f"/campaign/artifacts/seed-{seed}",
                "runspec_digest": f"sha256:{seed}",
                "verdict": "reject",
                "replay": "replay",
                "daemon": {"binary_sha256": "ab" * 32, "source_commit": commit},
                "subjects": ["path:register"],
                "properties": {},
                "obligations": [],
            }
            for seed in (3, 5)
        ],
        "body": "finding-01.md",
    }


def write_findings(
    directory: Path,
    entries: Sequence[dict[str, Any]] = (),
    checks: Sequence[str] = PASSING,
    **overrides: Any,
) -> Path:
    """Write a findings dir that `findings.read_findings` accepts, judged by the fake target."""
    document = {
        "schema_version": FINDINGS_SCHEMA_VERSION,
        "campaign_id": "sha256:campaign",
        "target": FAKE.name,
        "semantics_profile": FAKE.semantics_profile,
        "identity_digest": findings.current_identity(FAKE),
        "semantic_digest": semantic_digest(FAKE),
        "controls": {},
        "checks": list(checks),
        "findings": list(entries),
        "routed": [],
        "execution": [],
        "unclassified": [],
        **overrides,
    }
    directory.mkdir(parents=True, exist_ok=True)
    (directory / findings.FINDINGS_NAME).write_text(json.dumps(document))
    (directory / "finding-01.md").write_text("# [fake] bytes.torn-read\n\nbody\n")
    return directory


def replaying(original: set[str], replayed: set[str]) -> filing.Replay:
    """Return a replay whose runs yield these task keys."""

    def replay(run: Path, artifacts: Path) -> Replayed:
        return Replayed(
            (f"{run} -> {artifacts}",), frozenset(original), frozenset(replayed)
        )

    return replay


REPRODUCED: filing.Replay = replaying({TORN_KEY, THRIFT_KEY}, {TORN_KEY, THRIFT_KEY})


def refuse_replay(run: Path, artifacts: Path) -> Replayed:
    raise AssertionError(f"replayed {run} into {artifacts}")


def file(
    directory: Path,
    tasks: filing.Tasks,
    options: Options = OPTIONS,
    replay: filing.Replay = refuse_replay,
) -> tuple[int, list[str], dict[str, Any]]:
    code, lines = filing.file_findings(directory, FAKE, tasks, options, replay)
    return code, lines, json.loads((directory / filing.FILING_NAME).read_text())


def block(errors: list[Any] = TORN) -> str:
    return filing.render_key_block(filing.key_document(errors))


class FilingTest(unittest.TestCase):
    maxDiff = None

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def test_any_failed_gate_reads_and_files_nothing(self) -> None:
        cases = {
            "failed check": {
                "checks": [*PASSING, "FAIL red-control torn-write bytes.torn-read: x"]
            },
            "red controls not supplied": {
                "checks": [*PASSING, "red-controls: not supplied"]
            },
            "no checks": {"checks": []},
            "other registry": {"identity_digest": "sha256:other"},
            "other checker": {"semantic_digest": "sha256:other"},
        }
        for name, overrides in cases.items():
            with self.subTest(name):
                directory = write_findings(self.root / name, [entry()], **overrides)
                tasks = FakeTasks()
                code, lines, document = file(directory, tasks, FILE)
                self.assertEqual(1, code)
                self.assertEqual([], tasks.calls)
                self.assertTrue(lines[0].startswith("GATE FAIL "), lines)
                self.assertTrue(document["gates"])
                self.assertEqual([], document["decisions"])

    def test_routed_and_unclassified_findings_are_skipped(self) -> None:
        directory = write_findings(
            self.root,
            routed=[
                {
                    "seed": s,
                    "subject": "path:r",
                    "route": "harness",
                    "digest": "sha256:h",
                    "rules": ["x.y"],
                    "attributions": ["harness"],
                }
                for s in (1, 2)
            ],
            unclassified=[
                {
                    "seed": 4,
                    "subject": "path:r",
                    "property_id": "x",
                    "rule_id": "x.z",
                    "cause": "missing-role",
                }
            ],
        )
        tasks = FakeTasks()
        code, lines, _document = file(directory, tasks, FILE)
        self.assertEqual(0, code)
        self.assertEqual(
            [
                "SKIPPED(routed-harness; seeds [1, 2]) sha256:h",
                "SKIPPED(unclassified-missing-role; seeds [4]) x.z",
            ],
            lines[:2],
        )
        self.assertEqual([], tasks.writes())

    def test_a_new_key_is_created_under_the_parent_after_its_replay_reproduces_it(
        self,
    ) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks()
        tasks.add("T200", "Not a finding.\n\n```\nsome code\n```\n")
        replays: list[tuple[Path, Path]] = []

        def replay(run: Path, artifacts: Path) -> Replayed:
            replays.append((run, artifacts))
            return REPRODUCED(run, artifacts)

        code, lines, document = file(directory, tasks, FILE, replay)
        self.assertEqual(0, code)
        self.assertEqual(
            [
                (
                    Path("/campaign/artifacts/seed-3"),
                    directory / "replays" / TORN_KEY[7:23],
                )
            ],
            replays,
        )
        (draft,) = tasks.created
        self.assertEqual(
            (PARENT, "owner", filing.DEFAULT_TAG),
            (draft.parent, draft.owner, draft.tag),
        )
        self.assertEqual("[eden][exploratory] bytes.torn-read (torn-read)", draft.title)
        self.assertIn(block(), draft.description)
        self.assertIn("# [fake] bytes.torn-read\n\nbody\n", draft.description)
        self.assertIn("Artifacts: https://example/a", draft.description)
        self.assertIn("campaign `sha256:campaign`", draft.description)
        self.assertEqual(
            f"NEW {TORN_KEY}: no subtask carries it; replayed; created T901", lines[0]
        )
        self.assertEqual(["T901"], document["decisions"][0]["filed"])

    def test_a_replay_reproduces_a_key_only_when_both_runs_yield_it(self) -> None:
        cases = {
            "original only": replaying({TORN_KEY}, set()),
            "replay only": replaying(set(), {TORN_KEY}),
            "another key": replaying({TORN_KEY}, {THRIFT_KEY}),
        }
        for name, replay in cases.items():
            with self.subTest(name):
                directory = write_findings(self.root / name, [entry()])
                tasks = FakeTasks()
                code, lines, _document = file(directory, tasks, FILE, replay)
                self.assertEqual(0, code)
                self.assertEqual([], tasks.writes())
                self.assertTrue(lines[0].startswith("SKIPPED(replay "), lines[0])
                self.assertIn("did not reproduce the key", lines[0])

    def test_skip_replay_creates_without_replaying(self) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks()
        file(directory, tasks, replace(FILE, skip_replay=True))
        self.assertEqual(1, len(tasks.created))

    def test_a_closed_carrier_gets_a_comment_never_a_new_task(self) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks()
        tasks.add("T400", block(), closed=True)
        code, lines, _document = file(directory, tasks, FILE, REPRODUCED)
        self.assertEqual(0, code)
        self.assertEqual([], tasks.created)
        self.assertEqual([("comment", "T400")], tasks.writes())
        self.assertEqual(
            f"DUPLICATE {TORN_KEY} T400: carried by T400 (closed); commented on T400",
            lines[0],
        )

    def test_open_carriers_are_commented_and_closed_ones_only_when_none_is_open(
        self,
    ) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks()
        tasks.add("T401", block(), closed=True)
        tasks.add("T402", block())
        tasks.add("T403", f"Two keys.\n\n{block(THRIFT)}\n\n{block()}\n")
        _code, lines, _document = file(directory, tasks, FILE)
        self.assertEqual([("comment", "T402"), ("comment", "T403")], tasks.writes())
        self.assertEqual(
            f"DUPLICATE {TORN_KEY} T402, T403: carried by T401 (closed), T402 (open),"
            " T403 (open); commented on T402, T403",
            lines[0],
        )

        closed = FakeTasks()
        closed.add("T401", block(), closed=True)
        closed.add("T404", block(), closed=True)
        file(directory, closed, FILE)
        self.assertEqual([("comment", "T401"), ("comment", "T404")], closed.writes())

    def test_only_the_filing_owners_carriers_get_a_comment(self) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks()
        tasks.add("T405", block(), owner="teammate")
        tasks.add("T406", block())
        _code, lines, _document = file(directory, tasks, FILE)
        self.assertEqual([("comment", "T406")], tasks.writes())
        self.assertEqual(
            f"DUPLICATE {TORN_KEY} T406: carried by T405 (open), T406 (open);"
            " not commented on T405 (owned by teammate); commented on T406",
            lines[0],
        )

        theirs = FakeTasks()
        theirs.add("T405", block(), owner="teammate")
        code, _lines, _document = file(directory, theirs, FILE)
        self.assertEqual((0, [], []), (code, theirs.writes(), theirs.created))

    def test_more_new_keys_than_max_new_files_nothing(self) -> None:
        directory = write_findings(self.root, [entry(), entry(THRIFT)])
        tasks = FakeTasks()
        capped = replace(FILE, max_new=1)
        code, lines, document = file(directory, tasks, capped, refuse_replay)
        self.assertEqual(1, code)
        self.assertEqual([], tasks.writes())
        self.assertEqual("GATE FAIL max-new: 2 new keys, more than 1", lines[0])
        self.assertEqual(["max-new: 2 new keys, more than 1"], document["gates"])
        self.assertEqual(["NEW", "NEW"], [d["action"] for d in document["decisions"]])

        file(directory, tasks, replace(FILE, max_new=2), REPRODUCED)
        self.assertEqual(2, len(tasks.created))

    def test_one_sighting_comment_per_subtask_key_and_daemon_source_commit(
        self,
    ) -> None:
        tasks = FakeTasks()
        tasks.add("T300", f"Symptom.\n\n{block()}\n")
        first = write_findings(self.root / "first", [entry()])
        again = write_findings(self.root / "again", [entry()])
        newer = write_findings(self.root / "newer", [entry(commit="d" * 40)])
        _code, seen, _document = file(first, tasks, FILE)
        _code, repeated, _document = file(again, tasks, FILE)
        _code, rebuilt, _document = file(newer, tasks, FILE)
        self.assertEqual(
            [
                f"DUPLICATE {TORN_KEY} T300: carried by T300 (open); commented on T300",
                f"DUPLICATE {TORN_KEY} T300: carried by T300 (open); already commented"
                " on T300 for this source commit",
                f"DUPLICATE {TORN_KEY} T300: carried by T300 (open); commented on T300",
            ],
            [seen[0], repeated[0], rebuilt[0]],
        )
        first_note, newer_note = tasks.notes["T300"]
        self.assertTrue(
            first_note.startswith(f"edenfs-exploratory sighting {TORN_KEY} {COMMIT}\n")
        )
        self.assertTrue(
            newer_note.startswith(
                f"edenfs-exploratory sighting {TORN_KEY} {'d' * 40}\n"
            )
        )
        self.assertIn("/campaign/artifacts/seed-5", first_note)
        self.assertIn("campaign `sha256:campaign`", first_note)

    def test_predecision_read_failures_leave_a_fail_closed_receipt(self) -> None:
        for operation in ("subtasks", "task", "comments"):
            with self.subTest(operation=operation):
                directory = write_findings(self.root / operation, [entry()])
                tasks = FakeTasks()
                tasks.add("T300", block())
                failure = TasksError(f"{operation} lookup transport failed")
                with mock.patch.object(tasks, operation, side_effect=failure):
                    code, lines, document = file(directory, tasks, FILE)
                self.assertEqual(1, code)
                self.assertEqual([], tasks.writes())
                self.assertEqual([], document["decisions"])
                self.assertIn(str(failure), document["gates"][0])
                self.assertTrue(lines[0].startswith("GATE FAIL predecision:"), lines)
                self.assertEqual("sha256:campaign", document["campaign_id"])
                self.assertFalse(document["dry_run"])

    def test_the_receipt_exists_before_task_reads_and_replaces_stale_success(
        self,
    ) -> None:
        directory = write_findings(self.root, [entry()])
        file(directory, FakeTasks(), FILE, REPRODUCED)
        tasks = FakeTasks()

        def interrupted(parent: str) -> list[str]:
            document = json.loads((directory / filing.FILING_NAME).read_text())
            self.assertEqual(["filing: not completed"], document["gates"])
            self.assertEqual([], document["decisions"])
            self.assertEqual(PARENT, parent)
            raise KeyboardInterrupt()

        with mock.patch.object(tasks, "subtasks", interrupted):
            with self.assertRaises(KeyboardInterrupt):
                file(directory, tasks, FILE)
        self.assertEqual([], tasks.writes())

    def test_a_recovered_read_failure_does_not_keep_a_failed_receipt(self) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks()
        with mock.patch.object(tasks, "subtasks", side_effect=TasksError("lookup")):
            code, _lines, _document = file(directory, tasks, FILE)
        self.assertEqual(1, code)
        code, _lines, document = file(directory, tasks, FILE, REPRODUCED)
        self.assertEqual(0, code)
        self.assertEqual([], document["gates"])
        self.assertEqual(["T901"], document["decisions"][0]["filed"])

    def test_a_failed_write_is_recorded_and_the_rest_still_run(self) -> None:
        directory = write_findings(self.root, [entry(), entry(THRIFT)])
        tasks = FakeTasks(failing={"T300"})
        tasks.add("T300", block())
        code, lines, document = file(directory, tasks, FILE, REPRODUCED)
        self.assertEqual(1, code)
        self.assertEqual(1, len(tasks.created))
        by_key = {d["task_key"]: d for d in document["decisions"]}
        self.assertEqual(
            "TasksError: meta tasks.comment create exited 1: denied",
            by_key[TORN_KEY]["error"],
        )
        self.assertEqual(
            ("DUPLICATE", []), (by_key[TORN_KEY]["action"], by_key[TORN_KEY]["filed"])
        )
        self.assertEqual(
            (None, ["T901"]), (by_key[THRIFT_KEY]["error"], by_key[THRIFT_KEY]["filed"])
        )
        self.assertIn(
            f"ERROR {TORN_KEY}: TasksError: meta tasks.comment create exited 1: denied",
            lines,
        )
        self.assertTrue(
            lines[-1].startswith("filing: 2 keys, filed, 1 errors;"), lines[-1]
        )

    def test_a_dry_run_reads_but_never_writes_or_replays(self) -> None:
        directory = write_findings(self.root, [entry(), entry(THRIFT)])
        tasks = FakeTasks(read_only=True)
        tasks.add("T300", block())
        code, lines, document = file(directory, tasks)
        self.assertEqual(0, code)
        self.assertEqual([], tasks.writes())
        self.assertEqual(
            f"DUPLICATE {TORN_KEY} T300: carried by T300 (open); would comment on T300",
            lines[0],
        )
        self.assertIn(
            "would replay /campaign/artifacts/seed-3; would create a subtask of T100",
            lines[1],
        )
        self.assertTrue(document["dry_run"])
        self.assertEqual(
            ["DUPLICATE", "NEW"], [d["action"] for d in document["decisions"]]
        )
        self.assertTrue(
            lines[-1].endswith(
                f"dry run, nothing filed; wrote {directory / filing.FILING_NAME}"
            )
        )

    def test_findings_record_the_check_lines_they_print(self) -> None:
        artifacts = fake_campaign(self.root)
        lines = findings.write_findings(
            self.root / "report.json", artifacts, self.root / "findings", FAKE
        )
        document = findings.read_findings(
            self.root / "findings" / findings.FINDINGS_NAME
        )
        self.assertEqual(lines[: len(document["checks"])], document["checks"])
        self.assertEqual("red-controls: not supplied", document["checks"][-1])

    def test_the_file_verb_prints_and_records_a_dry_run(self) -> None:
        directory = write_findings(self.root, [entry()])
        tasks = FakeTasks(read_only=True)
        stdout = io.StringIO()
        with (
            mock.patch.object(filing, "MetaTasks", return_value=tasks) as adapter,
            redirect_stdout(stdout),
        ):
            code = _main(
                [
                    "file",
                    "--target",
                    "fake",
                    "--findings",
                    str(directory),
                    "--owner",
                    "owner",
                    "--act-as-crewmate",
                    "4098672120433366",
                ]
            )
        adapter.assert_called_once_with("4098672120433366")
        self.assertEqual(0, code)
        self.assertIn(
            f"NEW {TORN_KEY}: no subtask carries it; would replay", stdout.getvalue()
        )
        self.assertEqual(("subtasks", filing.FINDINGS_PARENT), tasks.calls[0])
        self.assertTrue((directory / filing.FILING_NAME).is_file())


class KeyBlockTest(unittest.TestCase):
    def test_a_rendered_block_parses_back_to_its_document(self) -> None:
        document = filing.key_document(TORN)
        description = (
            f"Prose  \nmore  \n\n{filing.render_key_block(document)}\n\nAfter.\n"
        )
        self.assertEqual(
            ({TORN_KEY: document}, []), filing.parse_key_blocks(description)
        )
        trailing = description.replace(TORN_KEY, TORN_KEY + "  ")
        self.assertEqual(({TORN_KEY: document}, []), filing.parse_key_blocks(trailing))

    def test_only_header_blocks_are_read_and_malformed_ones_are_named(self) -> None:
        good = block()
        header = f"{filing.KEY_BLOCK_HEADER} {TORN_KEY}"
        reordered = json.dumps(
            {"schema": "exploratory-error-set-v1", "errors": TORN[::-1] + TORN}
        )
        cases = {
            "wrong digest": good.replace(TORN_KEY, "sha256:" + "0" * 64),
            "extra line": good.replace("\n```", "\nextra\n```"),
            "not json": f"```\n{header}\n{{\n```",
            "not canonical": f"```\n{header}\n{reordered}\n```",
            "wrong schema": f"```\n{header}\n"
            + json.dumps({"schema": "other", "errors": TORN})
            + "\n```",
        }
        for name, text in cases.items():
            with self.subTest(name):
                keys, problems = filing.parse_key_blocks(f"intro\n{text}\n")
                self.assertEqual({}, keys)
                self.assertEqual(1, len(problems), problems)
        self.assertEqual(({}, []), filing.parse_key_blocks("```\nplain code\n```\n"))


# What `meta ... --output=json` prints for each command `MetaTasks` runs.
_META_OUTPUT: dict[tuple[str, str], Any] = {
    ("tasks.task", "describe"): {
        "subtasks": "T1, T2",
        "description": "d",
        "completed": "N/A",
        "owner": "someone",
    },
    ("tasks.task", "create"): {"number": "T3"},
    ("tasks.comment", "list"): [{"text": "c"}],
    ("tasks.comment", "create"): {"id": "1"},
}


_CAT_TIMEOUT = (
    "Failed to mint delegated CAT for CREWMATE\nCaused by:\n"
    "C++ runtime error during CAT request: (RECV_TIMEOUT) Receive Timeout "
    "for tier 'token.service'. TTransportException: Timed out\n"
    "Error detected but classified as infrastructure/transient"
)


class MetaTasksTest(unittest.TestCase):
    def test_only_read_calls_retry_the_observed_cat_mint_timeout(self) -> None:
        for operation, platform in (
            ("subtasks", "tasks.task"),
            ("task", "tasks.task"),
            ("comments", "tasks.comment"),
        ):
            with self.subTest(operation=operation):
                output = _META_OUTPUT[
                    (platform, "list" if operation == "comments" else "describe")
                ]
                failure = subprocess.CompletedProcess([], 1, "", _CAT_TIMEOUT)
                success = subprocess.CompletedProcess([], 0, json.dumps(output), "")
                with (
                    mock.patch.object(
                        subprocess, "run", side_effect=[failure, success]
                    ) as run,
                    mock.patch.object(filing.time, "sleep") as sleep,
                ):
                    tasks = filing.MetaTasks("crewmate")
                    result = getattr(tasks, operation)("T100")
                self.assertIsNotNone(result)
                self.assertEqual(2, run.call_count)
                first, second = run.call_args_list
                self.assertEqual(first, second)
                self.assertEqual(
                    ["meta", "--act-as-crewmate", "crewmate"], first.args[0][:3]
                )
                self.assertEqual(filing.READ_TIMEOUT_SECONDS, first.kwargs["timeout"])
                sleep.assert_called_once_with(filing.READ_RETRY_DELAY_SECONDS)

    def test_read_retries_are_bounded_and_leave_a_failure_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = write_findings(Path(temporary), [entry()])
            failure = subprocess.CompletedProcess([], 1, "", _CAT_TIMEOUT)
            with (
                mock.patch.object(subprocess, "run", return_value=failure) as run,
                mock.patch.object(filing.time, "sleep") as sleep,
            ):
                code, _lines, document = file(directory, filing.MetaTasks(), FILE)
            self.assertEqual(1, code)
            self.assertEqual(filing.READ_ATTEMPTS, run.call_count)
            self.assertEqual(filing.READ_ATTEMPTS - 1, sleep.call_count)
            self.assertEqual([], document["decisions"])
            self.assertIn("Failed to mint delegated CAT", document["gates"][0])

    def test_unrecognized_errors_denials_and_malformed_results_are_not_retried(
        self,
    ) -> None:
        cases = [
            (1, "", "unknown transport failure"),
            (1, "", "(RECV_TIMEOUT) Receive Timeout"),
            (1, "", _CAT_TIMEOUT + "\nAccess denied"),
            (1, "", _CAT_TIMEOUT + "\nMetaCLITrustException"),
            (1, '{"error":"denied"}', _CAT_TIMEOUT),
            (0, "not json", _CAT_TIMEOUT),
            (0, '{"error":"denied"}', ""),
            (
                1,
                "",
                "dCAT mint failed in transport: (RECV_TIMEOUT)\n"
                "Failed to mint delegated CAT: permission denied",
            ),
        ]
        for returncode, stdout, stderr in cases:
            with self.subTest(returncode=returncode, stdout=stdout, stderr=stderr):
                completed = subprocess.CompletedProcess([], returncode, stdout, stderr)
                with (
                    mock.patch.object(subprocess, "run", return_value=completed) as run,
                    mock.patch.object(filing.time, "sleep") as sleep,
                ):
                    with self.assertRaises(TasksError):
                        filing.MetaTasks().subtasks("T100")
                self.assertEqual(1, run.call_count)
                sleep.assert_not_called()

    def test_creates_and_comments_never_retry_even_a_cat_mint_timeout(self) -> None:
        for operation in ("create", "comment"):
            with self.subTest(operation=operation):
                failure = subprocess.CompletedProcess([], 1, "", _CAT_TIMEOUT)
                with (
                    mock.patch.object(subprocess, "run", return_value=failure) as run,
                    mock.patch.object(filing.time, "sleep") as sleep,
                ):
                    tasks = filing.MetaTasks()
                    with self.assertRaises(TasksError):
                        if operation == "create":
                            tasks.create(TaskDraft("t", "d", "owner", "T100", "tag"))
                        else:
                            tasks.comment("T100", "text")
                self.assertEqual(1, run.call_count)
                sleep.assert_not_called()

    def test_a_local_or_subprocess_timeout_failure_is_recorded_without_retry(
        self,
    ) -> None:
        for failure in (
            OSError("cannot start meta"),
            subprocess.TimeoutExpired(["meta", "tasks.task", "describe"], 60),
        ):
            with self.subTest(failure=failure):
                with tempfile.TemporaryDirectory() as temporary:
                    directory = write_findings(Path(temporary), [entry()])
                    with (
                        mock.patch.object(
                            subprocess, "run", side_effect=failure
                        ) as run,
                        mock.patch.object(filing.time, "sleep") as sleep,
                    ):
                        code, _lines, document = file(
                            directory, filing.MetaTasks(), FILE
                        )
                    self.assertEqual(1, code)
                    self.assertEqual([], document["decisions"])
                    self.assertIn(
                        "meta tasks.task describe failed", document["gates"][0]
                    )
                    self.assertEqual(1, run.call_count)
                    sleep.assert_not_called()

    def _argvs(self, tasks: filing.MetaTasks) -> list[list[str]]:
        """Run every `MetaTasks` call against a recording `subprocess.run`; return each argv."""
        argvs: list[list[str]] = []

        def run(argv: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
            argvs.append(argv)
            platform = next(a for a in argv[1:] if a.startswith("tasks."))
            output = _META_OUTPUT[(platform, argv[argv.index(platform) + 1])]
            return subprocess.CompletedProcess(argv, 0, json.dumps(output), "")

        with mock.patch.object(subprocess, "run", run):
            self.assertEqual(["T1", "T2"], tasks.subtasks("T100"))
            self.assertEqual(TaskRecord("T1", "d", False, "someone"), tasks.task("T1"))
            self.assertEqual(["c"], tasks.comments("T1"))
            self.assertEqual(
                "T3", tasks.create(TaskDraft("t", "d", "owner", "T100", "tag"))
            )
            tasks.comment("T1", "text")
        return argvs

    def test_a_crewmate_runs_every_call_with_the_global_flag_first(self) -> None:
        argvs = self._argvs(filing.MetaTasks("4098672120433366"))
        self.assertEqual(5, len(argvs))
        for argv in argvs:
            self.assertEqual(
                ["meta", "--act-as-crewmate", "4098672120433366"], argv[:3]
            )
            self.assertTrue(argv[3].startswith("tasks."), argv)

    def test_without_a_crewmate_no_call_carries_the_flag(self) -> None:
        argvs = self._argvs(filing.MetaTasks())
        self.assertEqual(5, len(argvs))
        for argv in argvs:
            self.assertEqual("meta", argv[0])
            self.assertTrue(argv[1].startswith("tasks."), argv)
            self.assertNotIn("--act-as-crewmate", argv)
