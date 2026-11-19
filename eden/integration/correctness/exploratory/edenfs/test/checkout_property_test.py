# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""The checkout property's interruption and activity rules: when an interrupted checkout
ends, statuses served while it lasts, and which checkout-only runs exercise the
target. Complements the interrupted and sl goto cases of the corpus family
`checkout`.
"""

from __future__ import annotations

import unittest
from dataclasses import replace

from eden.integration.correctness.exploratory.core import checker
from eden.integration.correctness.exploratory.edenfs.contracts.checkout import (
    IN_PROGRESS_RULE,
)
from eden.integration.correctness.exploratory.edenfs.target import EDENFS_TARGET
from eden.integration.correctness.exploratory.edenfs.test.judging import (
    judge,
    judge_isolated,
    obligations_of,
    status_of,
)
from eden.integration.correctness.exploratory.test.corpus import loader
from eden.integration.correctness.exploratory.test.corpus.author import (
    committed,
    R0,
    read_ok,
    Run,
    sl_status,
    thrift_checkout,
    thrift_status,
    TIP,
    value,
)


class PrestartFailureTest(unittest.TestCase):
    def _run(self) -> Run:
        """Synthetic six-action reduction of retained155476, not a recorded history."""
        run = Run("t", "prestart", "An exclusively prearmed FORCE fails before start")
        run.act(
            "a0",
            "n0",
            "fault_install",
            fault_id="F",
            site="checkout-begin",
            target_action="i0",
            behavior="error",
        )
        run.act(
            "a1",
            "i0",
            "checkout",
            revision=1,
            via="thrift",
            mode="force",
            requires=[("n0", "finished")],
        )
        run.act("a0", "n1", "fault_remove", fault_id="F", requires=[("i0", "finished")])
        run.act("a1", "i1", "status", channel="sl", requires=[("n1", "finished")])
        run.act("a1", "i3", "checkout", revision=1, via="goto", mode="continue")
        run.act("a1", "i6", "read", path=R0, channel="fuse")
        run.call("n0", "ok", fault_id="F")
        run.call(
            "i0",
            "fail",
            reason="checkout: fault",
            source="thrift",
            error_type="GENERIC_ERROR",
        )
        run.call("n1", "ok", fault_id="F", removed=0)
        run.call("i1", "ok", **sl_status({}))
        run.call(
            "i3",
            "fail",
            reason="goto: not in an interrupted goto state",
            returncode=255,
            stdout="",
            stderr="abort: not in an interrupted goto state\n",
        )
        run.call("i6", "ok", **read_ok(committed(R0, TIP)))
        run.finish()
        return run

    def test_prestart_failure_restores_old_state(self) -> None:
        verdict = judge(self._run())
        self.assertEqual("satisfied", status_of(verdict, "checkout.sl-abort", "i3"))
        self.assertEqual("satisfied", status_of(verdict, "bytes.stale-read", "i6"))
        refusal = obligations_of(verdict)[("checkout.interrupted-refusal", "i1")]
        self.assertEqual(
            [("action-kind", "status")], [(w.role, w.value) for w in refusal.witnesses]
        )
        self.assertEqual(
            "satisfied", str(refusal.status), "checkout.interrupted-refusal/i1"
        )
        self.assertEqual(checker.ACCEPT, verdict.kind)

    def test_prestart_checkout_cannot_produce_destination_bytes(self) -> None:
        run = self._run()
        destination = read_ok(committed(R0, 1))
        run.events[:] = [
            replace(event, data={**event.data, **destination})
            if event.type == "ok" and event.data.get("action") == "i6"
            else event
            for event in run.events
        ]
        verdict = judge(run)
        self.assertEqual("violated", status_of(verdict, "bytes.stale-read", "i6"))
        self.assertEqual(
            "satisfied", status_of(verdict, "checkout.interrupted-refusal", "i1")
        )


class PrestartDescriptorTest(unittest.TestCase):
    def _run(self, observed_old: bool, ordinal: int = 1) -> Run:
        """A held inode cannot be replaced by a checkout that failed before start."""
        run = Run("t", "prestart-fd", "Write overlaps failed checkout; read held fd")
        run.act("r", "o", "open", path=R0)
        run.act(
            "n",
            "f",
            "fault_install",
            fault_id="F",
            site="checkout-begin",
            target_action="c",
            behavior="error",
            ordinal=ordinal,
        )
        run.act(
            "c",
            "c",
            "checkout",
            revision=1,
            via="thrift",
            mode="force",
            requires=[("f", "finished")],
        )
        run.act("w", "w", "write", path=R0, value_id="W", requires=[("c", "started")])
        run.act("n", "x", "fault_remove", fault_id="F", requires=[("c", "finished")])
        run.act(
            "r", "r", "read_fd", fd="o", requires=[("w", "finished"), ("x", "finished")]
        )
        run.act("r", "z", "close", fd="o")
        run.call("o", "ok", fd=3)
        run.call("f", "ok", fault_id="F", ordinal=ordinal)
        run.inv("c")
        written = value("w", "W")
        run.call("w", "ok", bytes_written=len(written))
        run.done(
            "c",
            "fail",
            reason="checkout: failure",
            source="thrift",
            error_type="GENERIC_ERROR",
        )
        run.call("x", "ok", fault_id="F", removed=0 if ordinal == 1 else 1)
        run.call("r", "ok", **read_ok(committed(R0, TIP) if observed_old else written))
        run.call("z", "ok", fd=3)
        run.finish()
        return run

    def test_prestart_failure_does_not_hide_descriptor_write(self) -> None:
        for old, expected in ((True, "violated"), (False, "satisfied")):
            with self.subTest(observed_old=old):
                verdict = judge(self._run(old))
                result = obligations_of(verdict)[("fd.stale-read", "r")]
                self.assertEqual("descriptor:o", result.subject.token)
                self.assertEqual(expected, str(result.status))
                self.assertIsNone(result.not_evaluated_reason)

    def test_possible_started_checkout_keeps_descriptor_gap(self) -> None:
        for old in (True, False):
            with self.subTest(observed_old=old):
                verdict = judge(self._run(old, ordinal=2))
                result = obligations_of(verdict)[("fd.stale-read", "r")]
                self.assertEqual("not-evaluated", str(result.status))
                self.assertEqual("fd.unmodeled-object", result.not_evaluated_reason)


class InterruptionTest(unittest.TestCase):
    def _interrupted(self, name: str) -> Run:
        """c1 (Thrift to rev2) fails at an unknown phase."""
        run = Run("t", name, "c1 may be interrupted")
        run.act("a", "c1", "checkout", revision=2, via="thrift", mode="normal")
        run.call(
            "c1",
            "fail",
            reason="checkout: EdenError: failure",
            source="thrift",
            error_type="GENERIC_ERROR",
        )
        run.act("a", "c2", "checkout", revision=2, via="thrift", mode="normal")
        run.act("b", "s", "status", channel="thrift", revision=2)
        return run

    def test_a_resume_the_daemon_may_finish_ends_the_interruption(self) -> None:
        """A status served once a same-target checkout was invoked is legal."""
        shapes = {
            "ok-acked-after-status": (
                "ok",
                thrift_checkout(2, conflicts=0),
            ),
            "client-timeout": (
                "fail",
                {"reason": "TTransportException: TIMED_OUT", "source": "thrift"},
            ),
            "info": ("info", {}),
        }
        for name, (outcome, facts) in shapes.items():
            with self.subTest(shape=name):
                run = self._interrupted(name)
                run.inv("c2")
                run.call("s", "ok", **thrift_status({}, 2))
                run.done("c2", outcome, **facts)
                run.finish()
                verdict = judge_isolated(("checkout",), run)
                self.assertEqual(
                    "satisfied",
                    status_of(verdict, "checkout.interrupted-refusal", "s"),
                )
                self.assertNotEqual(checker.REJECT, verdict.kind)


_INTERRUPTED_ADVICE = (
    "a previous checkout was interrupted - please run `sl go 2` to resume it."
)


def _thrift_interrupted(name: str) -> Run:
    """c (Thrift to rev2) fails mid-walk with an EdenError."""
    run = Run("t", name, "c is interrupted")
    run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
    run.call(
        "c", "fail", reason="thrift: fault", source="thrift", error_type="GENERIC_ERROR"
    )
    return run


class InterruptedCellTest(unittest.TestCase):
    """The interruption is the `interrupted` cell: set by a failure, ended by a resume."""

    def _refused(self, target: int, via: str, text: str) -> checker.Verdict:
        run = _thrift_interrupted("refused")
        run.act("a", "d", "checkout", revision=target, via=via, mode="normal")
        if via == "thrift":
            run.call(
                "d",
                "fail",
                reason=f"thrift: {text}",
                source="thrift",
                error_type="CHECKOUT_IN_PROGRESS",
            )
        else:
            run.call(
                "d",
                "fail",
                reason="goto: exit 255",
                returncode=255,
                stdout="",
                stderr=f"abort: EdenError: {text}\n",
            )
        run.finish()
        return judge(run)

    def test_a_refusal_is_judged_by_its_wording(self) -> None:
        """EdenFS words a checkout refusal by its state (EdenMount.cpp:1757-1783)."""
        for target, via, text, expected in (
            (1, "goto", _INTERRUPTED_ADVICE, "satisfied"),
            (1, "thrift", _INTERRUPTED_ADVICE, "satisfied"),
            (2, "goto", _INTERRUPTED_ADVICE, "violated"),
            (2, "thrift", _INTERRUPTED_ADVICE, "violated"),
            (
                1,
                "goto",
                "another checkout operation is still in progress",
                "violated",
            ),
        ):
            with self.subTest(target=target, via=via, text=text):
                verdict = self._refused(target, via, text)
                self.assertEqual(expected, status_of(verdict, IN_PROGRESS_RULE, "d"))

    def test_a_status_refused_as_interrupted_needs_an_interruption(self) -> None:
        stderr = f"abort: EdenError: {_INTERRUPTED_ADVICE}\n"
        refused = {
            "reason": "status: exit 255",
            "returncode": 255,
            "stdout": "",
            "stderr": stderr,
        }
        for interrupted, expected in ((True, "satisfied"), (False, "violated")):
            with self.subTest(interrupted=interrupted):
                if interrupted:
                    run = _thrift_interrupted("status")
                else:
                    run = Run("t", "status", "an acked checkout")
                    run.act(
                        "a", "c", "checkout", revision=2, via="thrift", mode="normal"
                    )
                    run.call("c", "ok", **thrift_checkout(2, conflicts=0))
                run.act("a", "s", "status", channel="sl")
                run.call("s", "fail", **refused)
                run.finish()
                verdict = judge_isolated(("checkout", "core.input-legality"), run)
                self.assertEqual(
                    expected, status_of(verdict, "core.input-legality", "s")
                )

    def test_prestart_resume_preserves_the_prior_interruption(self) -> None:
        found = loader.case("checkout/interrupted-failed-resume-then-status-served")
        events = tuple(
            replace(
                event,
                type="fail",
                data={
                    "action": "s",
                    "actor": event.data["actor"],
                    "reason": _INTERRUPTED_ADVICE,
                    "source": "thrift",
                    "error_type": "CHECKOUT_IN_PROGRESS",
                },
            )
            if event.type == "ok" and event.data.get("action") == "s"
            else event
            for event in found.events()
        )
        verdict = checker.check(
            EDENFS_TARGET,
            found.spec(EDENFS_TARGET),
            events,
            execution={"status": "completed"},
        )
        self.assertEqual(checker.ACCEPT, verdict.kind)
        self.assertEqual("satisfied", status_of(verdict, "core.input-legality", "s"))


class ActivityTest(unittest.TestCase):
    def test_a_checkout_only_run_exercises_the_target(self) -> None:
        run = Run("t", "checkout-only", "one Thrift checkout")
        run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
        run.call("c", "ok", **thrift_checkout(2, conflicts=0))
        run.finish()
        verdict = judge_isolated(("checkout", "core.activity"), run)
        self.assertEqual("satisfied", status_of(verdict, "core.activity", "run"))
        self.assertEqual(checker.ACCEPT, verdict.kind)

    def test_an_sl_goto_is_judged_by_its_shape_rules(self) -> None:
        run = Run("t", "sl", "an sl goto to rev2 then a status")
        run.act("a", "c", "checkout", revision=2, via="goto", mode="normal")
        run.act("a", "s", "status", channel="sl")
        run.call("c", "ok", revision=2, node="0" * 40, via="goto")
        run.call("s", "ok", **sl_status({}))
        run.finish()
        verdict = judge_isolated(("checkout",), run)
        # Settled (no earlier move, nothing overlaps it): no order to judge.
        self.assertIsNone(status_of(verdict, "checkout.parent-before-paths", "c"))
        self.assertEqual("satisfied", status_of(verdict, "checkout.sl-abort", "c"))


class AbortIdentityTest(unittest.TestCase):
    """An illegal abort's key names the contradicted input."""

    def _abort(self, case: str) -> dict[str, tuple[str, dict[str, str]]]:
        found = loader.case(case)
        verdict = checker.check(
            EDENFS_TARGET,
            found.spec(EDENFS_TARGET),
            found.events(),
            execution={"status": "completed"},
        )
        assert verdict.report is not None
        return {
            item.scope: (
                str(item.status),
                {w.role: w.value for w in item.witnesses},
            )
            for result in verdict.report.results
            for item in result.obligations
            if item.rule_id == "checkout.sl-abort"
        }

    def test_a_listed_clean_path_has_one_identity_whatever_else_is_listed(
        self,
    ) -> None:
        not_blocking = {"outcome": "aborted", "blockers": "not-blocking"}
        self.assertEqual(
            {"c:listed": ("satisfied", not_blocking)},
            self._abort("checkout/sl-goto-missing-removed-aborts"),
        )
        for case in (
            "checkout/sl-goto-abort-lists-clean-path",
            "checkout/sl-goto-abort-lists-clean-path-beside-blocker",
        ):
            with self.subTest(case=case):
                self.assertEqual(
                    {"c:listed": ("violated", not_blocking)}, self._abort(case)
                )

    def test_a_successful_update_over_a_blocking_state_is_unlisted(self) -> None:
        found = self._abort("checkout/sl-goto-dirty-changed-succeeds")
        self.assertEqual(
            {"c": ("violated", {"outcome": "succeeded", "blockers": "unlisted"})},
            found,
        )
