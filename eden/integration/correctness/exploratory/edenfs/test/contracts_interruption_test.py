# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Interrupted checkouts: a typed rejection needs an overlapping or interrupting
checkout, which checkout failure is a known interruption, a resume's DRY_RUN
compares against the interrupted checkout's source, and `.hg/updatestate` decides
whether `--continue` may proceed.
"""

from __future__ import annotations

import unittest
from typing import Any

from eden.integration.correctness.exploratory.core.effects import (
    ABSENT,
    eq,
    Expect,
    none_of,
    ObligationRef,
    Observation,
    Window,
)
from eden.integration.correctness.exploratory.core.facts import HistoryFacts
from eden.integration.correctness.exploratory.core.history import Event
from eden.integration.correctness.exploratory.core.runspec import RunSpec
from eden.integration.correctness.exploratory.core.spec_types import SpecError
from eden.integration.correctness.exploratory.core.test.testing import ev
from eden.integration.correctness.exploratory.edenfs.contracts.checkout import (
    known_prestart,
    updatestate_cell,
)
from eden.integration.correctness.exploratory.edenfs.contracts.model import (
    DIR,
    EdenPinned,
    parent_cell,
    rev_value,
    SAPLING_PARENT,
)
from eden.integration.correctness.exploratory.edenfs.test.contracts_fixtures import (
    action_payload,
    checkout_op,
    checkout_spec,
    committed_value,
    contract_sink,
    engine_explains,
    guard_holds,
    is_explained,
    R0,
    read_obs,
    run_spec,
    sl_abort_facts,
    TIP,
)
from eden.integration.correctness.exploratory.edenfs.test.testing import VOCAB_TARGET


class RejectionContractTest(unittest.TestCase):
    _REJECTED = {
        "reason": "another checkout operation is still in progress",
        "source": "thrift",
        "error_type": "CHECKOUT_IN_PROGRESS",
    }
    _OK = {"revision": 1, "node": "n", "via": "thrift", "conflicts": 0}

    _INTERRUPTED_ADVICE = {
        **_REJECTED,
        "reason": "a previous checkout was interrupted - please run `sl go 1`",
    }

    def _spec(self, target: int = 2) -> RunSpec:
        return checkout_spec(
            ("c", checkout_op(1)),
            ("d", checkout_op(target)),
            actors=[{"id": "a", "actions": ["c"]}, {"id": "b", "actions": ["d"]}],
        )

    def test_the_in_progress_text_needs_an_overlapping_checkout(self) -> None:
        overlapping = [
            ev("invoke", "c", 1.0),
            ev("invoke", "d", 1.5),
            ev("fail", "d", 1.6, **self._REJECTED),
            ev("ok", "c", 2.0, **self._OK),
        ]
        self.assertTrue(is_explained(self._spec(), overlapping))
        alone = [
            ev("invoke", "c", 1.0),
            ev("ok", "c", 2.0, **self._OK),
            ev("invoke", "d", 3.0),
            ev("fail", "d", 3.1, **self._REJECTED),
        ]
        self.assertFalse(is_explained(self._spec(), alone))

    def test_the_interrupted_advice_needs_an_interruption_of_another_target(
        self,
    ) -> None:
        """Only another target is refused; EdenFS words it by its state."""
        failed = [
            ev("invoke", "c", 1.0),
            ev(
                "fail",
                "c",
                2.0,
                reason="x",
                source="thrift",
                error_type="GENERIC_ERROR",
            ),
            ev("invoke", "d", 3.0),
        ]
        advice = ev("fail", "d", 3.1, **self._INTERRUPTED_ADVICE)
        in_progress = ev("fail", "d", 3.1, **self._REJECTED)
        untyped = ev("fail", "d", 3.1, **{**self._REJECTED, "reason": "x"})
        self.assertTrue(is_explained(self._spec(), [*failed, advice]))
        # A Thrift checkout receives only beginCheckout's CHECKOUT_IN_PROGRESS
        # texts, so the rejection keys on `error_type` and any text is explained.
        self.assertTrue(is_explained(self._spec(), [*failed, untyped]))
        # EdenFS reported c's failure, so c no longer holds the guard.
        self.assertFalse(is_explained(self._spec(), [*failed, in_progress]))
        # The same target resumes instead.
        self.assertFalse(is_explained(self._spec(1), [*failed, advice]))
        acked = [
            ev("invoke", "c", 1.0),
            ev("ok", "c", 2.0, **self._OK),
            ev("invoke", "d", 3.0),
        ]
        self.assertFalse(is_explained(self._spec(), [*acked, advice]))

    def test_a_refused_sl_goto_may_have_held_the_guard_through_its_dry_run(
        self,
    ) -> None:
        """An `sl goto` past DRY_RUN holds the guard (`edenfs.rs:378-410`).

        `sl goto` c0 passed DRY_RUN, so it held the guard when Thrift c1
        was refused; Thrift c2 took the guard in c0's gap, so c0's NORMAL
        was refused too. Only c0 explains c1's refusal.
        """
        spec = checkout_spec(
            ("c0", checkout_op(1, via="goto")),
            ("c1", checkout_op(2)),
            ("c2", checkout_op(1)),
            actors=[
                {"id": "a", "actions": ["c0"]},
                {"id": "b", "actions": ["c1"]},
                {"id": "c", "actions": ["c2"]},
            ],
        )
        refused = {
            "reason": "goto",
            "returncode": 255,
            "stdout": "",
            "stderr": "abort: EdenError: another checkout operation is still in progress\n",
        }
        events = [
            ev("invoke", "c0", 1.0),
            ev("invoke", "c1", 1.1),
            ev("fail", "c1", 1.2, **self._REJECTED),
            ev("invoke", "c2", 1.3),
            ev("fail", "c0", 1.4, **refused),
            ev("ok", "c2", 1.5, **self._OK),
        ]
        self.assertTrue(is_explained(spec, events))

    def _interrupted(
        self,
        revision: int,
        via: str = "thrift",
        mode: str = "normal",
        *,
        installed: float = 0.1,
        reported: bool = True,
        ordinal: int | None = None,
    ) -> HistoryFacts:
        spec = run_spec(
            {
                "f": action_payload(
                    "fault_install",
                    fault_id="F",
                    site="checkout-begin",
                    target_action="c",
                    behavior="error",
                    requires=[["c", "started"]] if installed > 1.0 else [],
                    **({"ordinal": ordinal} if ordinal is not None else {}),
                ),
                "c": {
                    **checkout_op(1, via=via, mode=mode),
                    "requires": [] if installed > 1.0 else [["f", "finished"]],
                },
                "x": {
                    **action_payload("fault_remove", fault_id="F"),
                    "requires": [["c", "finished"]],
                },
                "d": {**checkout_op(revision), "requires": [["x", "finished"]]},
            },
            [{"id": "n", "actions": ["f", "x"]}, {"id": "a", "actions": ["c", "d"]}],
        )
        failure = {"reason": "x", "source": "thrift", "error_type": "GENERIC_ERROR"}
        if via != "thrift":
            failure = {
                "reason": "x",
                "returncode": 255,
                "stdout": "",
                "stderr": "abort: EdenError: fault\n",
            }
        events = [
            ev("invoke", "f", 1.05 if installed > 1.0 else 0.0),
            ev("ok", "f", installed, fault_id="F"),
            ev("invoke", "c", 1.0),
            ev("fail", "c", 2.0, **failure) if reported else ev("info", "c", 2.0),
            ev("invoke", "x", 2.1),
            ev("ok", "x", 2.2, fault_id="F", removed=1),
            ev("invoke", "d", 3.0),
            ev("ok", "d", 4.0, revision=revision, node="n", via="thrift", conflicts=0),
        ]
        return HistoryFacts.build(
            spec, sorted(events, key=lambda e: e.time), VOCAB_TARGET
        )

    def test_an_exclusively_armed_begin_error_is_prestart(self) -> None:
        for via, mode in (
            ("thrift", "normal"),
            ("thrift", "force"),
            ("goto", "force"),
            ("goto", "normal"),
        ):
            with self.subTest(via=via, mode=mode):
                facts = self._interrupted(1, via=via, mode=mode)
                self.assertTrue(known_prestart(facts, facts.action("c")))
        first = self._interrupted(1, ordinal=1)
        self.assertTrue(known_prestart(first, first.action("c")))

    def test_begin_error_phase_stays_ambiguous_without_exclusive_prearming(
        self,
    ) -> None:
        for facts in (
            self._interrupted(1, installed=1.1),
            self._interrupted(1, reported=False),
            self._interrupted(1, ordinal=2),
        ):
            with self.subTest(installed=facts.action("f").completed):
                self.assertFalse(known_prestart(facts, facts.action("c")))


class InterruptedCheckoutTest(unittest.TestCase):
    """Refusal of another target, resume of the same one, `--continue`."""

    def _interrupted(self, *, via: str = "thrift") -> list[Event]:
        failure = (
            {"reason": "thrift", "source": "thrift", "error_type": "GENERIC_ERROR"}
            if via == "thrift"
            else {
                "reason": "goto",
                "returncode": 255,
                "stdout": "",
                "stderr": "abort: EdenError: std::runtime_error: fault\n",
            }
        )
        return [ev("invoke", "c", 1.0), ev("fail", "c", 2.0, **failure)]

    def test_an_sl_goto_resets_parents_only_while_no_checkout_is_interrupted(
        self,
    ) -> None:
        """`set_parents` is refused once another target's checkout is
        interrupted (`resetParentCommits`), so an acked `sl goto` moved
        Sapling's parent before that interruption."""
        spec = checkout_spec(
            ("c", checkout_op(1)),
            ("d", checkout_op(2, via="goto")),
            actors=[{"id": "a", "actions": ["c"]}, {"id": "b", "actions": ["d"]}],
        )
        events = [
            ev("invoke", "d", 0.5),
            *self._interrupted(),
            ev("ok", "d", 3.0, revision=2, node="n", via="goto"),
            Event("run-closed", 9.0, {}),
        ]

        def parent(oid: str, key: str, atom: Any, lo: float) -> Observation:
            return Observation(
                id=oid,
                action=oid,
                cells=(parent_cell(key),),
                expect=Expect(((atom,),)),
                window=Window(lo, lo + 0.1),
                channel="thrift",
                obligations=(ObligationRef("status.parent", oid),),
            )

        # EdenFS's parent is c's target after c failed, so c's interruption happened.
        interrupted = parent("e", "", eq(rev_value(1)), 2.1)
        for lo, explained in ((0.6, True), (2.1, False)):
            with self.subTest(sapling_unmoved_at=lo):
                unmoved = parent("p", SAPLING_PARENT, none_of({rev_value(2)}), lo)
                self.assertEqual(
                    explained, is_explained(spec, events, (interrupted, unmoved))
                )

    def test_a_resume_abort_may_list_paths_modified_against_the_interrupted_source(
        self,
    ) -> None:
        """The resume's DRY_RUN diffs against the interrupted checkout's source (rev 3)."""
        spec = checkout_spec(
            ("c", checkout_op(1)),
            ("d", checkout_op(1, via="goto")),
            extra={"m": action_payload("chmod", path=R0, mode=0o755)},
            actors=[{"id": "a", "actions": ["c", "m", "d"]}],
        )
        events = [
            *self._interrupted(),
            ev("invoke", "m", 3.0),
            ev("ok", "m", 4.0, path=R0, mode=0o755),
            ev("invoke", "d", 5.0),
            ev("fail", "d", 6.0, **sl_abort_facts(1, R0)),
            Event("run-closed", 9.0, {}),
        ]
        moved = read_obs("p", R0, committed_value(R0, TIP), 7, 8)
        self.assertTrue(is_explained(spec, events, (moved,)))

    def test_a_resume_abort_may_list_a_directory_the_interruption_removed(
        self,
    ) -> None:
        """The listed directory is derived although no action names it.

        It is MISSING_REMOVED, which blocks, so it may be listed alone.
        """
        removed = "tree/a0/r3"
        spec = checkout_spec(
            ("c", checkout_op(1)),
            ("d", checkout_op(1, via="goto")),
            extra={"w": action_payload("write", path=R0, value_id="w")},
            actors=[{"id": "a", "actions": ["c", "w", "d"]}],
        )
        pinned = EdenPinned.for_spec(spec)
        self.assertEqual(
            (DIR, ABSENT),
            (pinned.committed(removed, TIP), pinned.committed(removed, 1)),
        )
        written = [ev("invoke", "w", 3.0), ev("ok", "w", 4.0, bytes_written=64)]
        for interrupted, listed, legal in (
            (True, (removed, R0), True),
            (True, (removed,), True),
            (False, (removed, R0), False),
        ):
            with self.subTest(interrupted=interrupted, listed=listed):
                failed = (
                    self._interrupted()
                    if interrupted
                    else [
                        ev("invoke", "c", 1.0),
                        ev(
                            "ok",
                            "c",
                            2.0,
                            revision=1,
                            node="n",
                            via="thrift",
                            conflicts=0,
                        ),
                    ]
                )
                events = [
                    *failed,
                    *written,
                    ev("invoke", "d", 5.0),
                    ev("fail", "d", 6.0, **sl_abort_facts(1, *listed)),
                    Event("run-closed", 9.0, {}),
                ]
                self.assertEqual(legal, is_explained(spec, events))

    def _continue(self, *between: tuple[str, dict[str, Any], list[Event]]) -> RunSpec:
        actions = {"c": checkout_op(1, via="goto")}
        for name, payload, _events in between:
            actions[name] = payload
        actions["k"] = checkout_op(1, via="goto", mode="continue")
        return run_spec(actions, [{"id": "a", "actions": list(actions)}])

    def test_continue_may_resume_an_sl_goto_refused_after_its_dry_run(
        self,
    ) -> None:
        """DRY_RUN wrote `.hg/updatestate`
        before the guard refused NORMAL, so `--continue` may proceed."""
        spec = self._continue()
        refused = [
            ev("invoke", "c", 1.0),
            ev(
                "fail",
                "c",
                2.0,
                reason="goto",
                returncode=255,
                stdout="",
                stderr="abort: EdenError: another checkout operation is still in progress\n",
            ),
        ]
        cont = [
            ev("invoke", "k", 3.0),
            ev("ok", "k", 4.0, revision=1, node="n", via="goto"),
            Event("run-closed", 9.0, {}),
        ]
        sink = contract_sink(spec, [*refused, *cont])
        groups = [
            group
            for group in sink.groups
            if any(effect.cell == updatestate_cell() for effect in group.effects)
        ]
        self.assertTrue(engine_explains(EdenPinned.for_spec(spec), groups, []))

    def test_a_refused_sl_goto_keeps_the_updatestate_file(self) -> None:
        """An attempt EdenFS's guard refused never deletes `.hg/updatestate`:
        an interrupted update stays resumable with `--continue` (T294157075)."""
        spec = self._continue()
        for stderr in (
            "abort: EdenError: another checkout operation is still in progress\n",
            "abort: EdenError: a previous checkout was interrupted - please run "
            "`sl go 2` to resume it.\n",
        ):
            events = [
                ev("invoke", "c", 1.0),
                ev(
                    "fail",
                    "c",
                    2.0,
                    reason="goto",
                    returncode=255,
                    stdout="",
                    stderr=stderr,
                ),
            ]
            clears = [
                effect
                for group in contract_sink(spec, events).groups
                for effect in group.effects
                if effect.cell == updatestate_cell() and effect.value == ABSENT
            ]
            with self.subTest(stderr=stderr):
                self.assertTrue(clears)
                for effect in clears:
                    assert effect.guard is not None
                    for value in (ABSENT, rev_value(1)):
                        self.assertFalse(
                            guard_holds(effect.guard, {updatestate_cell(): value})
                        )

    def test_a_continue_after_an_sl_attempt_to_another_revision_is_refused(
        self,
    ) -> None:
        """`--continue` goes where `.hg/updatestate` points (`cmdgoto`), and an
        unordered or earlier `sl go <other>` may leave its own destination
        there, so validation refuses the spec."""
        with self.assertRaisesRegex(SpecError, "checkout --continue requires"):
            self._continue(("d", checkout_op(2, via="goto"), []))
        self._continue(("d", checkout_op(1, via="goto"), []))
