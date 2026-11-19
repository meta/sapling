# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""The campaign report: its JSON shape, the run dirs that survive their seed, and the
input families with whether each was witnessed, which the health check requires.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import random
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from eden.integration.correctness.exploratory.cli import _daemon_identity, _main
from eden.integration.correctness.exploratory.core import containment, history
from eden.integration.correctness.exploratory.core.campaign import (
    run_campaign,
    WITNESS_FLOOR,
)
from eden.integration.correctness.exploratory.core.containment import (
    COMPLETED,
    ExplorationResult,
    HARNESS_FAILURE,
)
from eden.integration.correctness.exploratory.core.runspec import (
    from_json,
    RunSpec,
    to_json,
)
from eden.integration.correctness.exploratory.edenfs.target import (
    sample_environment as sample_edenfs_environment,
)
from eden.integration.correctness.exploratory.fake.target import FAKE
from eden.integration.correctness.exploratory.targets import target_entries
from eden.integration.correctness.exploratory.test.cli_fixtures import cli_argv
from eden.integration.correctness.exploratory.test.tasking_fixtures import (
    exploration_result,
)


# The CLI's EdenFS entry; generation, strata, and sites need only its vocabulary.
EDENFS_TARGET = target_entries()["edenfs"].target


class CampaignReportTest(unittest.TestCase):
    @mock.patch.object(containment, "run_exploration")
    def test_report_json_shape(self, run_exploration: mock.Mock) -> None:
        reason = "bytes/stale-read: observed old bytes"
        result = exploration_result(COMPLETED, "reject", reason, "seed-0")
        run_exploration.return_value = result
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report_path = root / "report.json"
            exit_code = _main(
                cli_argv(
                    "campaign",
                    target="fake",
                    seeds=1,
                    artifacts=root / "artifacts",
                    report=report_path,
                )
            )

            report = json.loads(report_path.read_text())
            daemon = root / "edenfs"
            daemon.write_bytes(b"stack-built daemon")
            with mock.patch.dict(os.environ, {"EDENFS_SERVER_PATH": str(daemon)}):
                daemon_identity = _daemon_identity("edenfs")

        self.assertEqual(1, exit_code)
        self.assertEqual(
            {
                "schema_version",
                "campaign_id",
                "completed",
                "daemon_identity",
                "excluded_sites",
                "properties",
                "environment_degraded",
                "harness_failures",
                "input_families",
                "rate",
                "leaked_run_dirs",
                "rejects",
                "seeds",
                "sites",
                "strata",
                "under_witnessed_families",
                "actions",
                "obligations",
                "verdicts",
                "work",
                "not_evaluated_causes",
            },
            report.keys(),
        )
        self.assertEqual({}, report["sites"])
        self.assertEqual({}, report["input_families"])
        self.assertEqual([], report["under_witnessed_families"])
        self.assertEqual("fake", report["daemon_identity"])
        self.assertEqual([], report["leaked_run_dirs"])
        self.assertEqual(0, report["harness_failures"])
        self.assertEqual(0, report["environment_degraded"])
        self.assertEqual(0.0, report["rate"])
        self.assertEqual({}, report["properties"])
        (reject,) = report["rejects"]
        self.assertEqual((0, reason), (reject["seed"], reject["reason"]))
        self.assertTrue(reject["artifacts"].endswith("seed-0"))
        self.assertEqual(
            (1, 1, {"reject": 1}),
            (report["seeds"], report["completed"], report["verdicts"]),
        )
        self.assertEqual({"completed": {}, "missing": []}, report["strata"])
        self.assertEqual(
            f"sha256:{hashlib.sha256(b'stack-built daemon').hexdigest()}",
            daemon_identity,
        )

    def test_report_lists_run_dirs_that_survived_their_seed(self) -> None:
        """`leaked_run_dirs` names each seed whose recorded run dir still exists."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def runner(spec: RunSpec, path: str, _target: Any) -> ExplorationResult:
                run_dir = root / f"run.{spec.seed}"
                run_dir.mkdir()
                if spec.seed != 500:
                    run_dir.rmdir()
                return exploration_result(
                    COMPLETED, "accept", "ok", path, setup={"run_dir": str(run_dir)}
                )

            report = run_campaign(
                FAKE, 2, 500, lambda _seed: {}, str(root / "a"), runner, "public-master"
            )
            written = json.loads((root / "a" / "report.json").read_text())
        expected = [{"seed": 500, "path": str(root / "run.500")}]
        self.assertEqual(tuple(expected), report.leaked_run_dirs)
        self.assertEqual(expected, written["leaked_run_dirs"])

    def _family_spec(self) -> RunSpec:
        """One fault spec holding a readdir stream across i0 with a lease before removal."""
        environment = dict(sample_edenfs_environment(random.Random(0)))
        environment["setup_profile"] = {
            "materialize": False,
            "scatter_dirty": 0,
            "wide_dir": {"name": "tree/wide", "entries": 2000, "churn_all": True},
        }
        after_i0 = [["i0", "finished"]]
        actions: dict[str, dict[str, Any]] = {
            "n0": {
                "kind": "fault_install",
                "fault_id": "F",
                "site": "checkout-begin",
                "target_action": "i0",
                "behavior": "error",
            },
            "l0": {
                "kind": "background_read",
                "requires": after_i0,
                "root": "tree",
                "seconds": 2.0,
            },
            "n1": {"kind": "fault_remove", "requires": after_i0, "fault_id": "F"},
            "d0": {"kind": "readdir_stream", "path": "tree/wide", "pause_after": 1000},
            "i0": {
                "kind": "checkout",
                "requires": [["n0", "finished"], ["d0", "finished"]],
                "revision": 2,
                "via": "goto",
                "mode": "normal",
            },
            "d1": {"kind": "readdir_resume", "stream": "d0"},
        }
        document = {
            "schema": "edenfs-exploratory-run",
            "schema_version": 1,
            "seed": 0,
            "target": "edenfs",
            "environment": {"target": environment},
            "budgets": {
                "operation_deadline_seconds": 120,
                "cleanup_deadline_seconds": 30,
                "runner_deadline_seconds": 600,
            },
            "actors": [
                {"id": "a0", "actions": ["n0", "l0", "n1"]},
                {"id": "a1", "actions": ["d0", "i0", "d1"]},
            ],
            "actions": {
                name: {"deadline": "operation", **action}
                for name, action in actions.items()
            },
        }
        return from_json(json.dumps(document), EDENFS_TARGET)

    @staticmethod
    def _write_family_run(
        spec: RunSpec,
        path: str,
        *,
        paused_early: bool,
        resumed: bool,
        landed: bool,
        stream_failed: bool = False,
        resume_failed: bool = False,
        lease_info: bool = False,
    ) -> None:
        """Write a run whose stream and lease witnesses are chosen per flag."""
        times = {
            "n0": (0.0, 1.0),
            "d0": (1.0, 2.0 if paused_early else 4.0),
            "i0": (3.0, 5.0),
            "l0": (5.5, 7.5) if landed else (7.0, 9.0),
            "n1": (6.0, 6.5),
            "d1": (6.0, 7.0),
        }
        failed = {"d0": stream_failed, "d1": resume_failed}
        events = []
        for action, (invoke, done) in times.items():
            events.append({"type": history.INVOKE, "time": invoke, "action": action})
            if action == "l0" and lease_info:
                events.append(
                    {
                        "type": history.INFO,
                        "time": done,
                        "action": action,
                        "reason": "r",
                    }
                )
            elif action != "d1" or resumed:
                outcome = history.FAIL if failed.get(action) else history.OK
                events.append({"type": outcome, "time": done, "action": action})
        os.makedirs(path, exist_ok=True)
        Path(path, "run-spec.json").write_text(to_json(spec))
        Path(path, "history.jsonl").write_text(
            "".join(
                json.dumps(e) + "\n" for e in sorted(events, key=lambda e: e["time"])
            )
        )

    def _family_campaign(self, seeds: int, runs: dict[int, dict[str, bool]]) -> Any:
        """Run a campaign of fixed specs; `runs` chooses each seed's witnesses and status."""
        spec = self._family_spec()

        def runner(_spec: RunSpec, path: str, _target: Any) -> ExplorationResult:
            flags = dict(runs.get(_spec.seed) or runs[-1])
            status = HARNESS_FAILURE if flags.pop("failed", False) else COMPLETED
            self._write_family_run(spec, path, **flags)
            return exploration_result(status, "accept", "ok", path)

        with (
            tempfile.TemporaryDirectory() as artifact_dir,
            mock.patch(
                "eden.integration.correctness.exploratory.core.campaign.compose",
                side_effect=lambda _t, seed, *_a, **_k: dataclasses.replace(
                    spec, seed=seed
                ),
            ),
        ):
            report = run_campaign(
                EDENFS_TARGET,
                seeds,
                0,
                lambda _s: {},
                artifact_dir,
                runner,
                "public-master",
            )
            document = json.loads((Path(artifact_dir) / "report.json").read_text())
        return report, document

    def test_report_lists_input_families(self) -> None:
        """Per family: specs carrying it, completed runs, and witnessed runs.

        A stream is witnessed when it paused before its partner was invoked and
        resumed after the partner completed; a lease when its interval overlaps
        a nemesis window other than its own.
        """
        witnessed = {"paused_early": True, "resumed": True, "landed": True}
        report, document = self._family_campaign(
            3,
            {
                0: witnessed,
                1: {**witnessed, "paused_early": False},
                2: {**witnessed, "resumed": False, "landed": False, "failed": True},
            },
        )
        expected = {
            "background_read": {
                "specs": 3,
                "completed": 2,
                "witnessed": 2,
                "exit_unobserved": 0,
            },
            "readdir_stream": {
                "specs": 3,
                "completed": 2,
                "witnessed": 1,
                "exit_unobserved": 0,
            },
        }
        self.assertEqual(expected, report.input_families)
        self.assertEqual(expected, document["input_families"])

    def test_lease_completing_info_counts_its_exit_unobserved(self) -> None:
        """An `info` lease is witnessed only by another window; its exit is counted unobserved.

        Its own window never closes, so it must not witness the lease itself.
        """
        witnessed = {"paused_early": True, "resumed": True, "landed": True}
        report, document = self._family_campaign(
            3,
            {
                0: {**witnessed, "lease_info": True},
                1: {**witnessed, "lease_info": True, "landed": False},
                2: witnessed,
            },
        )
        expected = {"specs": 3, "completed": 3, "witnessed": 2, "exit_unobserved": 2}
        self.assertEqual(expected, report.input_families["background_read"])
        self.assertEqual(expected, document["input_families"]["background_read"])
        self.assertEqual(0, report.input_families["readdir_stream"]["exit_unobserved"])

    def test_stream_not_witnessed_unless_pause_and_resume_complete_ok(self) -> None:
        """A resume or stream that completes `fail` in the right order is not a witness.

        A failed resume means the paused child exited instead of draining after
        the partner; a failed stream never reported `paused_at`.
        """
        witnessed = {"paused_early": True, "resumed": True, "landed": True}
        report, document = self._family_campaign(
            2,
            {
                0: {**witnessed, "resume_failed": True},
                1: {**witnessed, "stream_failed": True},
            },
        )
        expected = {"specs": 2, "completed": 2, "witnessed": 0, "exit_unobserved": 0}
        self.assertEqual(expected, report.input_families["readdir_stream"])
        self.assertEqual(expected, document["input_families"]["readdir_stream"])
        self.assertEqual(2, report.input_families["background_read"]["witnessed"])

    def test_report_lists_every_under_witnessed_input_family(self) -> None:
        """A family under the witness floor is listed, not gated; the campaign still runs out."""
        witnessed = {"paused_early": True, "resumed": True, "landed": True}
        runs = {seed: witnessed for seed in range(WITNESS_FLOOR - 1)}
        runs[-1] = {**witnessed, "paused_early": False}
        report, document = self._family_campaign(55, runs)
        self.assertEqual(55, report.attempts)
        self.assertIsNone(report.stopped)
        self.assertEqual(
            WITNESS_FLOOR - 1, report.input_families["readdir_stream"]["witnessed"]
        )
        self.assertEqual(55, report.input_families["background_read"]["witnessed"])
        self.assertEqual(("readdir_stream",), report.under_witnessed_families)
        self.assertEqual(["readdir_stream"], document["under_witnessed_families"])
