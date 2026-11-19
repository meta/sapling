# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""The checkout conflict count: a Thrift checkout's reported conflict count is one the
contract per-path states allow. Complements the conflict-count rows of the corpus
family `checkout`: these tests pin the measured conflict table, when the count is decided directly and when the engine
decides it, racing writes and checkouts, FORCE counting, and the recorded live count.
"""

from __future__ import annotations

import itertools
import random
import unittest
from typing import Any

from eden.integration.correctness.exploratory.core import checker, effects
from eden.integration.correctness.exploratory.core.facts import HistoryFacts
from eden.integration.correctness.exploratory.core.runspec import from_json
from eden.integration.correctness.exploratory.edenfs.contracts import (
    checkout as contracts_checkout,
    model as contracts_model,
)
from eden.integration.correctness.exploratory.edenfs.contracts.checkout import (
    CONTRACT_RULES,
    named_paths,
)
from eden.integration.correctness.exploratory.edenfs.target import EDENFS_TARGET
from eden.integration.correctness.exploratory.edenfs.test.judging import (
    contract_run,
    judge,
    judge_isolated,
    obligations_of,
    status_of,
)
from eden.integration.correctness.exploratory.edenfs.test.recorded_count import (
    count_outcome,
    recorded_verdict,
)
from eden.integration.correctness.exploratory.edenfs.test.testing import getattr_facts
from eden.integration.correctness.exploratory.test.corpus.author import (
    committed,
    R0,
    R1,
    read_ok,
    Run,
    sl_abort,
    sl_checkout,
    thrift_checkout,
    TIP,
    value,
    WRITE_OK,
)


class ConflictCountTest(unittest.TestCase):
    def test_conflict_states_follow_the_measured_table(self) -> None:
        """Which local states each mode reports, per path shape."""
        file_a = contracts_model._Entry(
            contracts_model.file_value(b"a"), contracts_model.mode_value(False)
        )
        file_b = contracts_model._Entry(
            contracts_model.file_value(b"b"), contracts_model.mode_value(False)
        )
        absent = contracts_model._Entry(effects.ABSENT, effects.ABSENT)
        local_c = (
            contracts_model.file_value(b"c"),
            contracts_model.mode_value(False),
            contracts_model.CLEAN,
        )
        exec_a = (
            contracts_model.file_value(b"a"),
            contracts_model.mode_value(True),
            contracts_model.CLEAN,
        )
        local_a = (file_a.value, file_a.mode, contracts_model.CLEAN)
        local_b = (file_b.value, file_b.mode, contracts_model.CLEAN)
        gone = (effects.ABSENT, effects.ABSENT, contracts_model.CLEAN)

        def conflicts(
            force: bool,
            source: contracts_model._Entry,
            target: contracts_model._Entry,
            local: tuple[effects.Value, ...],
        ) -> bool:
            rows = contracts_checkout._conflict_states(force)(source, target)
            return any(
                all(atom.matches(value) for atom, value in zip(row, local))
                for row in rows
            )

        cases = [
            # (force, S, T, local, conflict)
            (False, file_a, file_b, local_a, False),  # clean changed
            (False, file_a, file_b, local_b, False),  # dirty equal to destination
            (False, file_a, file_b, local_c, True),  # MODIFIED_MODIFIED
            (False, file_a, file_b, exec_a, True),  # mode-only dirt
            (False, file_a, absent, local_c, True),  # MODIFIED_REMOVED
            (False, file_a, file_b, gone, True),  # REMOVED_MODIFIED
            (False, file_a, absent, gone, True),  # MISSING_REMOVED
            (False, absent, file_b, local_b, True),  # UNTRACKED_ADDED (equal)
            (False, absent, file_b, gone, False),  # clean add
            (False, file_a, file_a, local_c, False),  # NORMAL unchanged
            (True, file_a, file_a, local_c, True),  # FORCE dirty unchanged
            (True, file_a, file_a, gone, True),  # FORCE removed unchanged
            (True, file_a, file_a, local_a, False),  # FORCE clean unchanged
            (True, file_a, file_b, local_a, False),  # FORCE clean changed
            (True, absent, absent, local_c, False),  # in neither revision
        ]
        for force, source, target, local, expected in cases:
            with self.subTest(force=force, local=local):
                self.assertEqual(expected, conflicts(force, source, target, local))

    def _count(self, conflicts: int, *, dirty: bool, listing: bool = False) -> Run:
        run = Run("t", "count", "Thrift NORMAL to rev2")
        if dirty:
            run.act("a", "w", "write", path=R0, value_id="w")
        run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
        if listing:
            run.act("a", "l", "readdir", path="tree/registers", channel="fuse")
        if dirty:
            run.call("w", "ok", **WRITE_OK)
        run.call("c", "ok", **thrift_checkout(2, conflicts=conflicts))
        if listing:
            names = ["r0.reg", "r1.reg", "r2.reg"]
            run.call("l", "ok", channel="fuse", entries=names)
        run.finish()
        return run

    def test_a_count_with_no_possible_conflict_is_judged_directly(self) -> None:
        """Untouched clean paths track the parent: they get no marker."""
        spec = from_json(self._count(0, dirty=False).spec_text(), EDENFS_TARGET)
        self.assertEqual(frozenset(), named_paths(spec))
        clean = judge_isolated(("checkout",), self._count(0, dirty=False))
        self.assertEqual("satisfied", status_of(clean, "checkout.conflict-count", "c"))
        claimed = judge_isolated(("checkout",), self._count(1, dirty=False))
        self.assertEqual("violated", status_of(claimed, "checkout.conflict-count", "c"))
        derived = contract_run(self._count(1, dirty=False), CONTRACT_RULES)
        self.assertEqual([], [o for o in derived.observations if "conflicts" in o.id])

    def _prestart_count(
        self, conflicts: int, ordinal: int = 1, via: str = "thrift"
    ) -> Run:
        """Synthetic clean fixture: a first-hit failure restores state (D129789564)."""
        run = Run("t", "prestart-count", "Failed checkout then counted NORMAL")
        run.act(
            "n",
            "f",
            "fault_install",
            fault_id="F",
            site="checkout-begin",
            target_action="c1",
            behavior="error",
            ordinal=ordinal,
        )
        run.act(
            "a",
            "c1",
            "checkout",
            revision=1,
            via=via,
            mode="force" if via == "thrift" else "normal",
            requires=[("f", "finished")],
        )
        run.act("n", "x", "fault_remove", fault_id="F", requires=[("c1", "finished")])
        run.act(
            "a",
            "c",
            "checkout",
            revision=2,
            via="thrift",
            mode="normal",
            requires=[("x", "finished")],
        )
        run.call("f", "ok", fault_id="F", ordinal=ordinal)
        failure = (
            {"source": "thrift", "error_type": "GENERIC_ERROR"}
            if via == "thrift"
            else {
                "returncode": 255,
                "stdout": "",
                "stderr": "abort: EdenError: fault\n",
            }
        )
        run.call("c1", "fail", reason="checkout: fault", **failure)
        run.call("x", "ok", fault_id="F", removed=0 if ordinal == 1 else 1)
        run.call("c", "ok", **thrift_checkout(2, conflicts=conflicts))
        run.finish()
        return run

    def test_known_prestart_failure_keeps_clean_counts_decidable(self) -> None:
        for conflicts, expected in ((0, "satisfied"), (1, "violated")):
            with self.subTest(conflicts=conflicts):
                run = self._prestart_count(conflicts)
                spec = from_json(run.spec_text(), EDENFS_TARGET)
                facts = HistoryFacts.build(spec, run.events, EDENFS_TARGET)
                self.assertTrue(
                    contracts_checkout.known_prestart(facts, facts.action("c1"))
                )
                result = obligations_of(judge(run))[("checkout.conflict-count", "c")]
                self.assertEqual("checkout:c", result.subject.token)
                self.assertEqual(
                    [("mode", "normal")], [(w.role, w.value) for w in result.witnesses]
                )
                self.assertEqual(expected, str(result.status))
                self.assertIsNone(result.not_evaluated_reason)

    def test_unknown_phase_keeps_count_gap_and_compound_goto_state(self) -> None:
        for via in ("thrift", "goto"):
            with self.subTest(via=via):
                run = self._prestart_count(0, ordinal=2, via=via)
                spec = from_json(run.spec_text(), EDENFS_TARGET)
                facts = HistoryFacts.build(spec, run.events, EDENFS_TARGET)
                self.assertFalse(
                    contracts_checkout.known_prestart(facts, facts.action("c1"))
                )
                result = obligations_of(judge(run))[("checkout.conflict-count", "c")]
                self.assertEqual("not-evaluated", str(result.status))
                self.assertEqual(
                    "checkout.conflict-count-unmodeled", result.not_evaluated_reason
                )
                if via == "goto":
                    self.assertTrue(
                        any(
                            effect.cell == contracts_checkout.updatestate_cell()
                            and effect.value == contracts_model.rev_value(1)
                            for group in contract_run(run).groups
                            if group.action == "c1"
                            for effect in group.effects
                        )
                    )

    def test_a_settled_dirty_register_is_counted_by_the_engine(self) -> None:
        """The write ends before the checkout starts; the count is still an
        engine observation over R0's marker (no settled shortcut)."""
        derived = contract_run(self._count(1, dirty=True), CONTRACT_RULES)
        (count,) = [o for o in derived.observations if "conflicts" in o.id]
        self.assertEqual(1, len(count.cells))
        for conflicts, expected in ((1, "satisfied"), (0, "violated"), (2, "violated")):
            with self.subTest(conflicts=conflicts):
                verdict = judge_isolated(
                    ("checkout",), self._count(conflicts, dirty=True)
                )
                self.assertEqual(
                    expected, status_of(verdict, "checkout.conflict-count", "c")
                )

    def _racing_writes(self, conflicts: int) -> Run:
        """Two writes to R0 overlap each other, then a NORMAL checkout to rev2."""
        run = Run("t", "count", "racing writes, then Thrift NORMAL to rev2")
        run.act("a", "w1", "write", path=R0, value_id="w1")
        run.act("b", "w2", "write", path=R0, value_id="w2")
        run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
        run.inv("w1", "w2")
        run.done("w1", "ok", **WRITE_OK)
        run.done("w2", "ok", **WRITE_OK)
        run.call("c", "ok", **thrift_checkout(2, conflicts=conflicts))
        run.finish()
        return run

    def test_a_racing_register_is_counted_by_the_engine(self) -> None:
        """Racing writes leave R0's order open: the engine reads the marker."""
        derived = contract_run(self._racing_writes(1), CONTRACT_RULES)
        (count,) = [o for o in derived.observations if o.id == "o:c:conflicts"]
        self.assertEqual(1, len(count.cells))
        self.assertTrue(
            all(
                str(item.obligation.rule_id) == "checkout.conflict-count"
                for item in derived.constraints
            )
        )
        for conflicts, expected in ((1, "satisfied"), (0, "violated"), (2, "violated")):
            with self.subTest(conflicts=conflicts):
                verdict = judge_isolated(("checkout",), self._racing_writes(conflicts))
                self.assertEqual(
                    expected, status_of(verdict, "checkout.conflict-count", "c")
                )

    def _force_race(self, conflicts: int) -> Run:
        """A dirty R0; a FORCE checkout to rev3 may run before or after a NORMAL one to rev2."""
        run = Run("t", "count", "FORCE to rev3 racing NORMAL to rev2 over a dirty R0")
        run.act("a", "w", "write", path=R0, value_id="w")
        run.act("a", "f", "checkout", revision=TIP, via="thrift", mode="force")
        run.act("b", "c", "checkout", revision=2, via="thrift", mode="normal")
        run.call("w", "ok", **WRITE_OK)
        run.inv("f")
        run.inv("c")
        run.done("f", "ok", **thrift_checkout(TIP, conflicts=1))
        run.done("c", "ok", **thrift_checkout(2, conflicts=conflicts))
        run.finish()
        return run

    def test_a_checkout_that_may_land_first_leaves_the_count_to_the_engine(
        self,
    ) -> None:
        """If `f` resets R0 first, `c` finds it clean (0); otherwise dirty (1)."""
        derived = contract_run(self._force_race(0), CONTRACT_RULES)
        self.assertEqual(
            ["o:c:conflicts"], [o.id for o in derived.observations if o.action == "c"]
        )
        for conflicts, expected in (
            (0, "satisfied"),
            (1, "satisfied"),
            (2, "violated"),
        ):
            with self.subTest(conflicts=conflicts):
                verdict = judge_isolated(("checkout",), self._force_race(conflicts))
                self.assertEqual(
                    expected, status_of(verdict, "checkout.conflict-count", "c")
                )

    def _source_race(self, first: int, second: int) -> Run:
        """A dirty R0; overlapping NORMAL checkouts to rev2 (`c1`) and rev3 (`c2`).

        `c1` keeps R0 and reports it either way. If `c1` moves the parent
        first, `c2` starts from rev2 and reports R0 too; if `c2` runs
        first, it starts from rev3, its own destination, and examines
        nothing. So `c2` may count 1 or 0, and only the source decides.
        """
        run = Run("t", "count", "overlapping NORMAL checkouts over a dirty R0")
        run.act("a", "w", "write", path=R0, value_id="w")
        run.act("a", "c1", "checkout", revision=2, via="thrift", mode="normal")
        run.act("b", "c2", "checkout", revision=TIP, via="thrift", mode="normal")
        run.call("w", "ok", **WRITE_OK)
        run.inv("c1")
        run.inv("c2")
        run.done("c1", "ok", **thrift_checkout(2, conflicts=first))
        run.done("c2", "ok", **thrift_checkout(TIP, conflicts=second))
        run.finish()
        return run

    def test_a_checkout_that_may_move_the_parent_first_leaves_the_source_open(
        self,
    ) -> None:
        derived = contract_run(self._source_race(1, 1), CONTRACT_RULES)
        self.assertEqual(
            ["o:c2:conflicts"],
            [o.id for o in derived.observations if o.action == "c2"],
        )
        for counts, expected in (
            ((1, 1), "accept"),
            ((1, 0), "accept"),
            ((0, 0), "reject"),
            ((1, 2), "reject"),
        ):
            with self.subTest(counts=counts):
                self.assertEqual(
                    expected,
                    str(judge_isolated(("checkout",), self._source_race(*counts)).kind),
                )

    def _r14(self, conflicts: int, data: bytes) -> Run:
        """Thrift NORMAL to rev2 racing one write to R0, then a read of R0."""
        run = Run("t", "count-decoupled", "a write racing NORMAL, then a read")
        run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
        run.act("b", "w", "write", path=R0, value_id="w")
        run.act(
            "o",
            "g",
            "read",
            path=R0,
            channel="fuse",
            requires=[("c", "finished"), ("w", "finished")],
        )
        run.inv("c", "w")
        run.done("w", "ok", **WRITE_OK)
        run.done("c", "ok", **thrift_checkout(2, conflicts))
        run.call("g", "ok", **read_ok(data))
        run.finish()
        return run

    def _kept_dirt(self, conflicts: int) -> Run:
        """A dirty R0; `k` (NORMAL to rev3, the start) overlaps `c` (NORMAL to rev2)."""
        run = Run("t", "count", "a same-revision checkout racing a counted one")
        run.act("a", "w", "write", path=R0, value_id="w")
        run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
        run.act("b", "k", "checkout", revision=TIP, via="thrift", mode="normal")
        run.call("w", "ok", **WRITE_OK)
        run.inv("c")
        run.inv("k")
        run.done("c", "ok", **thrift_checkout(2, conflicts=conflicts))
        run.done("k", "ok", **thrift_checkout(TIP, conflicts=1))
        run.finish()
        return run

    def test_a_racing_checkout_that_cannot_change_a_path_keeps_it_dirty(
        self,
    ) -> None:
        """Whenever `k` lands, its guards keep the dirty R0, so `c` finds it
        dirty (1); the engine decides it."""
        derived = contract_run(self._kept_dirt(1), CONTRACT_RULES)
        self.assertEqual(
            ["o:c:conflicts"], [o.id for o in derived.observations if o.action == "c"]
        )
        for conflicts, expected in ((1, "satisfied"), (0, "violated")):
            with self.subTest(conflicts=conflicts):
                self.assertEqual(
                    expected,
                    status_of(
                        judge_isolated(("checkout",), self._kept_dirt(conflicts)),
                        "checkout.conflict-count",
                        "c",
                    ),
                )

    def _same_revision(self, conflicts: int, shape: str) -> tuple[Run, str]:
        """NORMAL Thrift checkouts to the revision the mount is at, with writes.

        The legal-simulator shapes an earlier count the contracts accepted at count
        1 (seeds 76, 81, 271, and checkout `c1` of seed 25). A NORMAL
        checkout skips every path whose source and destination
        entries match, so the legal count is 0 whatever the writes do.
        """
        run = Run("t", "count", f"NORMAL to the current revision ({shape})")
        counted = "c"
        if shape == "seed-76":
            run.act("a", "c", "checkout", revision=TIP, via="thrift", mode="normal")
            run.act("b", "w5", "write", path=R0, value_id="v5")
            run.act("c", "w2", "write", path=R1, value_id="v2")
            run.inv("c")
            run.inv("w5")
            run.done("c", "ok", **thrift_checkout(TIP, conflicts=conflicts))
            run.done("w5", "ok", **WRITE_OK)
            run.call("w2", "ok", **WRITE_OK)
        elif shape == "seed-81":
            run.act("a", "w4", "write", path=R1, value_id="v4")
            run.act("b", "c", "checkout", revision=TIP, via="thrift", mode="normal")
            run.act("c", "w0", "write", path=R1, value_id="v0")
            run.act("d", "w5", "write", path=R0, value_id="v5")
            run.inv("w4")
            run.inv("c")
            run.done("w4", "ok", **WRITE_OK)
            run.inv("w0")
            run.done("c", "ok", **thrift_checkout(TIP, conflicts=conflicts))
            run.done("w0", "ok", **WRITE_OK)
            run.call("w5", "ok", **WRITE_OK)
        elif shape == "seed-271":
            run.act("a", "c", "checkout", revision=TIP, via="thrift", mode="normal")
            run.act("b", "w0", "write", path=R0, value_id="v0")
            run.inv("c")
            run.inv("w0")
            run.done("c", "ok", **thrift_checkout(TIP, conflicts=conflicts))
            run.done("w0", "ok", **WRITE_OK)
        else:
            assert shape == "seed-25-c1"
            counted = "c1"
            run.act("a", "c0", "checkout", revision=TIP, via="thrift", mode="force")
            run.act("b", "c1", "checkout", revision=TIP, via="thrift", mode="normal")
            run.act("c", "w2", "write", path=R1, value_id="v2")
            run.inv("c0")
            run.inv("w2")
            run.done("c0", "ok", **thrift_checkout(TIP, conflicts=1))
            run.inv("c1")
            run.done("w2", "ok", **WRITE_OK)
            run.done("c1", "ok", **thrift_checkout(TIP, conflicts=conflicts))
        run.finish()
        return run, counted

    def _force_then_read(self, conflicts: int, read: str) -> Run:
        """FORCE to the current revision racing a write of R0, then a read of R0.

        The legal-simulator seed-133 shape: count 1 needs the write to
        land before FORCE visits R0, and FORCE then resets R0, so a later
        read of the written bytes contradicts count 1; count 0 puts the
        write after the visit, where it is kept or lands in the inode
        FORCE replaced, so either read is legal.
        """
        run = Run("t", "count", "FORCE racing a write, then a read")
        run.act("a", "c", "checkout", revision=TIP, via="thrift", mode="force")
        run.act("b", "w", "write", path=R0, value_id="w")
        run.act("a", "r", "read", path=R0, channel="fuse")
        run.inv("c")
        run.inv("w")
        run.done("c", "ok", **thrift_checkout(TIP, conflicts=conflicts))
        run.done("w", "ok", **WRITE_OK)
        data = value("b", "w") if read == "written" else committed(R0, TIP)
        run.call("r", "ok", **read_ok(data))
        run.finish()
        return run

    def test_force_counts_a_racing_write_only_if_it_lands_first(self) -> None:
        """A write on a path FORCE's destination keeps is ordered by the
        engine against FORCE's visit instead of widening the count."""
        derived = contract_run(self._force_then_read(1, "written"), CONTRACT_RULES)
        self.assertEqual(
            ["o:c:conflicts"], [o.id for o in derived.observations if o.action == "c"]
        )
        for conflicts, read, expected in (
            (1, "written", "violated"),
            (0, "written", "satisfied"),
            (1, "committed", "satisfied"),
            (0, "committed", "satisfied"),
        ):
            with self.subTest(conflicts=conflicts, read=read):
                verdict = judge(self._force_then_read(conflicts, read))
                self.assertEqual(
                    expected, status_of(verdict, "checkout.conflict-count", "c")
                )
                self.assertEqual(
                    "reject" if expected == "violated" else "accept",
                    str(verdict.kind),
                )


class SupersedeTest(unittest.TestCase):
    """A checkout supersedes each overlapping write or chmod on its own.

    An op whose open or lookup resolved the old inode before the swap
    lands in the unlinked inode: the cell it set ends at the destination's
    and no conflict is reported. FORCE swaps every path either revision
    holds, NORMAL and `sl goto` only paths the destination changes.
    Every register changes between revisions, so a checkout to the
    current revision (TIP) stands for a path the destination keeps.
    """

    def _race(
        self,
        checkout: dict[str, Any],
        ops: tuple[tuple[str, str], ...],
        done: dict[str, Any],
        final: dict[str, tuple[bytes, bool]],
        *,
        early: bool = False,
    ) -> Run:
        """Race `ops` (kind, path) with one checkout, then read and getattr `final`.

        With `early`, every op completes before the checkout starts.
        """
        run = Run("t", "supersede", "a checkout racing writes and chmods")
        run.act("a", "c", "checkout", **checkout)
        ids = []
        for index, (kind, path) in enumerate(ops):
            op = f"o{index}"
            ids.append(op)
            if kind == "write":
                run.act(f"b{index}", op, "write", path=path, value_id=op)
            else:
                run.act(f"b{index}", op, "chmod", path=path, mode=0o755)
        after = [("c", "finished"), *((op, "finished") for op in ids)]
        for index, path in enumerate(final):
            run.act("r", f"r{index}", "read", requires=after, path=path, channel="fuse")
            run.act("r", f"g{index}", "getattr", requires=after, path=path)

        def complete() -> None:
            for index, (kind, path) in enumerate(ops):
                if kind == "write":
                    run.done(f"o{index}", "ok", **WRITE_OK)
                else:
                    run.done(f"o{index}", "ok", path=path, mode=0o755)

        if early:
            run.inv(*ids)
            complete()
            run.inv("c")
        else:
            run.inv("c", *ids)
            complete()
        run.done("c", "ok" if done.get("returncode") is None else "fail", **done)
        for index, (data, executable) in enumerate(final.values()):
            run.call(f"r{index}", "ok", **read_ok(data))
            run.call(
                f"g{index}", "ok", **getattr_facts(mode=0o755 if executable else 0o644)
            )
        run.finish()
        return run

    def _assert_verdicts(
        self, cases: list[tuple[str, Run, str]], rules: tuple[str, ...] = ()
    ) -> None:
        for name, run, expected in cases:
            with self.subTest(name):
                verdict = judge(run)
                self.assertEqual(expected, str(verdict.kind))
                if expected == "accept":
                    for rule in rules:
                        self.assertIn(
                            status_of(verdict, rule, "c"), ("satisfied", None)
                        )

    @staticmethod
    def _thrift(mode: str, revision: int = 2) -> dict[str, object]:
        return {"revision": revision, "via": "thrift", "mode": mode}

    def test_force_may_lose_a_write_with_no_conflict_on_changed_and_kept_paths(
        self,
    ) -> None:
        """Count 0 with the destination's bytes is legal under FORCE."""
        force = self._thrift("force")
        mine = value("b0", "o0")
        cases = [
            (
                "changed, lost, count 0",
                self._race(
                    force,
                    (("write", R0),),
                    thrift_checkout(2, 0),
                    {R0: (committed(R0, 2), False)},
                ),
                "accept",
            ),
            (
                "kept, lost, count 0",
                self._race(
                    self._thrift("force", TIP),
                    (("write", R0),),
                    thrift_checkout(TIP, 0),
                    {R0: (committed(R0, TIP), False)},
                ),
                "accept",
            ),
            (
                "changed, landed first, count 1",
                self._race(
                    force,
                    (("write", R0),),
                    thrift_checkout(2, 1),
                    {R0: (committed(R0, 2), False)},
                ),
                "accept",
            ),
            (
                "changed, counted yet kept",
                self._race(
                    force,
                    (("write", R0),),
                    thrift_checkout(2, 1),
                    {R0: (mine, False)},
                ),
                "reject",
            ),
            (
                "a conflict no racing op explains",
                self._race(
                    force,
                    (("write", R0),),
                    thrift_checkout(2, 2),
                    {R0: (committed(R0, 2), False)},
                ),
                "reject",
            ),
            (
                "a write done before the checkout, count 0",
                self._race(
                    force,
                    (("write", R0),),
                    thrift_checkout(2, 0),
                    {R0: (committed(R0, 2), False)},
                    early=True,
                ),
                "reject",
            ),
        ]
        self._assert_verdicts(cases, ("checkout.conflict-count",))

    def test_a_chmod_alone_may_be_lost(self) -> None:
        """Chmod is superseded like a write, under NORMAL and FORCE."""
        lost = {R0: (committed(R0, 2), False)}
        cases = [
            (
                f"{mode}, changed, lost",
                self._race(
                    self._thrift(mode), (("chmod", R0),), thrift_checkout(2, 0), lost
                ),
                "accept",
            )
            for mode in ("normal", "force")
        ]
        cases += [
            (
                "force, kept, lost",
                self._race(
                    self._thrift("force", TIP),
                    (("chmod", R0),),
                    thrift_checkout(TIP, 0),
                    {R0: (committed(R0, TIP), False)},
                ),
                "accept",
            ),
            (
                "normal, kept, lost",
                self._race(
                    self._thrift("normal", TIP),
                    (("chmod", R0),),
                    thrift_checkout(TIP, 0),
                    {R0: (committed(R0, TIP), False)},
                ),
                "reject",
            ),
            (
                "normal, done before the checkout, lost",
                self._race(
                    self._thrift("normal"),
                    (("chmod", R0),),
                    thrift_checkout(2, 1),
                    lost,
                    early=True,
                ),
                "reject",
            ),
        ]
        self._assert_verdicts(cases, ("checkout.conflict-count",))

    def test_a_write_and_a_chmod_resolve_independently(self) -> None:
        """All four lost/kept combinations are legal, in both modes."""
        written = value("b0", "o0")
        destination = committed(R0, 2)
        cases = [
            (
                f"{mode}, write {'kept' if data == written else 'lost'}, "
                f"chmod {'kept' if executable else 'lost'}",
                self._race(
                    self._thrift(mode),
                    (("write", R0), ("chmod", R0)),
                    thrift_checkout(2, 0),
                    {R0: (data, executable)},
                ),
                "accept",
            )
            for mode in ("normal", "force")
            for data in (written, destination)
            for executable in (True, False)
        ]
        self._assert_verdicts(cases, ("checkout.conflict-count",))

    def test_a_lost_chmod_needs_no_racing_write_on_its_path(self) -> None:
        """Chmod R0 lost, write R0 kept, write R1 counted and kept."""
        run = self._race(
            self._thrift("normal"),
            (("chmod", R0), ("write", R0), ("write", R1)),
            thrift_checkout(2, 1),
            {R0: (value("b1", "o1"), False), R1: (value("b2", "o2"), False)},
        )
        self._assert_verdicts([("c3", run, "accept")], ("checkout.conflict-count",))

    def test_sl_goto_may_keep_lose_or_abort_on_a_racing_write(self) -> None:
        """Exit 0 keeping or losing the write, or an abort listing it."""
        update = {"revision": 2, "via": "goto", "mode": "normal"}
        written = value("b0", "o0")
        cases = [
            (
                "exit 0, kept",
                self._race(
                    update, (("write", R0),), sl_checkout(2), {R0: (written, False)}
                ),
                "accept",
            ),
            (
                "exit 0, lost",
                self._race(
                    update,
                    (("write", R0),),
                    sl_checkout(2),
                    {R0: (committed(R0, 2), False)},
                ),
                "accept",
            ),
            (
                "abort listing it",
                self._race(
                    update, (("write", R0),), sl_abort([R0]), {R0: (written, False)}
                ),
                "accept",
            ),
            (
                "kept path, exit 0, lost",
                self._race(
                    {**update, "revision": TIP},
                    (("write", R0),),
                    sl_checkout(TIP),
                    {R0: (committed(R0, TIP), False)},
                ),
                "reject",
            ),
        ]
        self._assert_verdicts(cases, ("checkout.sl-abort",))


class RecordedConflictCountTest(unittest.TestCase):
    def test_the_recorded_count_is_accepted(self) -> None:
        verdict = recorded_verdict(0)
        self.assertEqual("accept", str(verdict.kind))
        self.assertEqual(("satisfied", None), count_outcome(verdict))


# One write or chmod of a register: (kind, path, bytes or exec bit).
_Op = tuple[str, str, object]


def _possible_counts(
    target: int, mode: str, ops: list[_Op], places: list[str]
) -> set[int]:
    """Every conflict count a Thrift checkout from TIP may report (independent model).

    Each register is visited once inside the checkout; an op placed
    `before` lands before every visit (in index order), `after` after
    every visit, `overlap` anywhere. NORMAL conflicts at a path the
    destination changes whose local (bytes, exec) is neither the source
    nor the destination entry; FORCE at any path whose local differs from
    the source entry.
    """
    source = {path: committed(path, TIP) for path in (R0, R1)}
    destination = {path: committed(path, target) for path in (R0, R1)}
    events = [("op", index) for index in range(len(ops))] + [
        ("visit", path) for path in (R0, R1)
    ]
    counts = set()
    for order in itertools.permutations(events):
        position = {event: index for index, event in enumerate(order)}
        visits = [position[("visit", path)] for path in (R0, R1)]
        before = [i for i, place in enumerate(places) if place == "before"]
        if any(
            (places[i] == "before" and position[("op", i)] > min(visits))
            or (places[i] == "after" and position[("op", i)] < max(visits))
            for i in range(len(ops))
        ) or any(
            position[("op", a)] > position[("op", b)]
            for a, b in zip(before, before[1:])
        ):
            continue
        local: dict[str, tuple[Any, Any]] = {
            path: (source[path], False) for path in (R0, R1)
        }
        count = 0
        for kind, item in order:
            if kind == "op":
                op_kind, path, data = ops[int(item)]
                held, executable = local[path]
                local[path] = (data, executable) if op_kind == "write" else (held, data)
                continue
            path = str(item)
            clean = (source[path], False)
            if mode == "normal":
                if source[path] != destination[path] and local[path] not in (
                    clean,
                    (destination[path], False),
                ):
                    count += 1
            elif local[path] != clean:
                count += 1
        counts.add(count)
    return counts


def _count_history(
    seed: int, conflicts: int
) -> tuple[Run, int, str, list[_Op], list[str]]:
    """A Thrift checkout from TIP with up to three writes or chmods of r0 and r1,
    each finished before it, overlapping it, or after it."""
    rng = random.Random(seed)
    target = rng.choice((0, 1, 2, TIP))
    mode = rng.choice(("normal", "force"))
    run = Run("t", f"count-{seed}", "random ops around a Thrift checkout")
    ops: list[_Op] = []
    places: list[str] = []
    for index in range(rng.randint(0, 3)):
        name, path = f"o{index}", rng.choice((R0, R1))
        if rng.random() < 0.6:
            run.act(f"b{index}", name, "write", path=path, value_id=name)
            ops.append(("write", path, value(f"b{index}", name)))
        else:
            executable = rng.random() < 0.7
            run.act(
                f"b{index}",
                name,
                "chmod",
                path=path,
                mode=0o755 if executable else 0o644,
            )
            ops.append(("chmod", path, executable))
        places.append(rng.choice(("before", "overlap", "after")))
    run.act("a", "c", "checkout", revision=target, via="thrift", mode=mode)

    def finish(index: int) -> None:
        kind, path, data = ops[index]
        if kind == "write":
            run.done(f"o{index}", "ok", **WRITE_OK)
        else:
            run.done(f"o{index}", "ok", path=path, mode=0o755 if data else 0o644)

    for index in (i for i, place in enumerate(places) if place == "before"):
        run.inv(f"o{index}")
        finish(index)
    overlapping = [i for i, place in enumerate(places) if place == "overlap"]
    run.inv("c")
    if overlapping:
        run.inv(*(f"o{index}" for index in overlapping))
        for index in overlapping:
            finish(index)
    run.done("c", "ok", **thrift_checkout(target, conflicts=conflicts))
    for index in (i for i, place in enumerate(places) if place == "after"):
        run.inv(f"o{index}")
        finish(index)
    run.finish()
    return run, target, mode, ops, places


class NoMarkerCountParityTest(unittest.TestCase):
    """The direct "no marker => count 0" decision (`checkout._judge_counts`,
    `contracts.checkout.count_judgment`) against an independent model of the counts.

    Whenever the contracts finds no conflict marker (and no gap), every order of
    the ops and the per-path visits the model allows reports 0 conflicts,
    and the checker accepts 0 and rejects 1.
    """

    def test_no_marker_means_only_zero_is_legal(self) -> None:
        unmarked = marked = raced = 0
        for seed in range(400):
            run, target, mode, ops, places = _count_history(seed, 0)
            spec = from_json(run.spec_text(), EDENFS_TARGET)
            facts = checker.HistoryFacts.build(
                spec, tuple(run.events), EDENFS_TARGET, execution_status="completed"
            )
            assert EDENFS_TARGET.pinned is not None
            judgment = contracts_checkout.count_judgment(
                spec, facts, EDENFS_TARGET.pinned(spec), "c"
            )
            if judgment.gap is not None:
                continue
            if judgment.markers:
                marked += 1
                continue
            unmarked += 1
            raced += any(place != "after" for place in places)
            with self.subTest(seed=seed, target=target, mode=mode, places=places):
                self.assertEqual({0}, _possible_counts(target, mode, ops, places))
                self.assertEqual(
                    "satisfied",
                    status_of(
                        judge_isolated(("checkout",), run),
                        "checkout.conflict-count",
                        "c",
                    ),
                )
                claimed, *_ = _count_history(seed, 1)
                self.assertEqual(
                    "violated",
                    status_of(
                        judge_isolated(("checkout",), claimed),
                        "checkout.conflict-count",
                        "c",
                    ),
                )
        counts = f"unmarked={unmarked} (raced {raced}) marked={marked}"
        self.assertGreater(unmarked, 60, counts)
        self.assertGreater(raced, 20, counts)
        self.assertGreater(marked, 60, counts)
