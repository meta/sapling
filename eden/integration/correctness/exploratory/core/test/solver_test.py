# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""The decision engine on hand-built problems.

Worked examples from the design, violations that survive budget-limited
neighbors, proof cores under explanation budgets, components coupled by a
shared outcome variable, unknown initial values and candidate caps that never
contradict, final reads, residual proofs, count observations read after
every writer, per-component budget shares, and the groups a proof names.
"""

from __future__ import annotations

import dataclasses
import itertools
import math
import unittest

from eden.integration.correctness.exploratory.core import exhaustive, solver
from eden.integration.correctness.exploratory.core.effects import (
    ABSENT,
    Agree,
    ANY,
    Before,
    Cell,
    constraint_id,
    Count,
    eq,
    Exclusive,
    Expect,
    Guard,
    ObligationRef,
    Observation,
    PRIMARY,
    Supersedes,
    Value,
    Window,
)
from eden.integration.correctness.exploratory.core.property import (
    ObligationStatus,
    WorkAccount,
)
from eden.integration.correctness.exploratory.core.semantics import explains, Problem
from eden.integration.correctness.exploratory.core.test.problems import (
    cell,
    checkout_problem,
    counted_problem,
    group,
    hard_component,
    history_problem,
    lost_write_problem,
    make_problem,
    NOT_EVALUATED,
    observe,
    planted_mixed_history,
    produce,
    random_problem,
    SATISFIED,
    sees,
    shared_variable_problem,
    transfer,
    V1,
    V2,
    VIOLATED,
)


P, A, B = cell("p"), cell("fileA"), cell("fileB")


T = Value("file", ("checkout-target",))


W = Value("file", ("write",))


BYTES = ObligationRef("t.bytes", "p")


STATUS = ObligationRef("t.status", "p")


def _status(solution: solver.Solution, ref: ObligationRef) -> ObligationStatus:
    return solution.outcomes[ref].status


def _proof(solution: solver.Solution, ref: ObligationRef):
    outcome = solution.outcomes[ref]
    return next(p for p in solution.proofs if p.id == outcome.proofs[0])


SMALL = solver.Budgets(
    decision_work=40_000, explanation_work=80_000, max_live_states=100_000
)


DIRTY = Value("file", ("dirty",))


MARK = Value("yes")


COUNT = ObligationRef("t.count", "c")


def _counted(paths: int, extra: int = 0, *, ordered: bool = True) -> Problem:
    """A counted settled checkout over `paths` paths, reporting `extra` too many conflicts.

    Each path's group writes the destination over a clean path and marks a
    conflict over a dirty one (every even path); a read after the group
    sees the result. One count at the groups' end reads every marker and is
    ordered after each group (unless not `ordered`).
    """
    groups, reads, orderings = [], [], []
    initial: dict[Cell, Value] = {}
    markers = tuple(Cell(PRIMARY, "marker", f"m{i}") for i in range(paths))
    for i, marker in enumerate(markers):
        target = cell(f"p{i}")
        dirty = i % 2 == 0
        initial[target] = DIRTY if dirty else V1
        initial[marker] = ABSENT
        groups.append(
            group(
                f"g{i}",
                0,
                2,
                produce(target, T, Guard((target,), Expect(((eq(V1),),)))),
                produce(marker, MARK, Guard((target,), Expect(((eq(DIRTY),),)))),
            )
        )
        seen = DIRTY if dirty else T
        reads.append(
            sees(f"o{i}", 3, 4, target, seen, ObligationRef("t.read", f"o{i}"))
        )
        if ordered:
            orderings.append(Before(f"g{i}", "count", COUNT))
    total = (paths + 1) // 2 + extra
    count = Observation(
        "count",
        "count",
        markers,
        Expect(count=Count(MARK, tuple((1, 1) for _ in markers), total)),
        Window(2, 2),
        "thrift",
        (COUNT,),
    )
    return make_problem(groups, [*reads, count], orderings, initial)


class WorkedExampleTest(unittest.TestCase):
    """Four small histories encoded as engine problems.

    5.1 a force checkout and a write ordered both ways; 5.2 an unacked
    write (recorded INFO), which may or may not have applied; 5.3
    overlapping checkout attempts under `Exclusive`; 5.4 a write
    overlapping a rename, each applied atomically.
    """

    def test_5_1_force_checkout_and_write_cycle(self) -> None:
        checkout = group("c", 0, 10, produce(P, T))
        write = group("w", 2, 4, produce(P, W))
        # Bytes keep the write visible; status says the checkout superseded it.
        cycle = make_problem(
            [checkout, write],
            [],
            [Supersedes("c", "w", BYTES), Supersedes("w", "c", STATUS)],
            {P: ABSENT},
        )
        solution = solver.solve(cycle)
        self.assertEqual(
            (VIOLATED, VIOLATED), (_status(solution, BYTES), _status(solution, STATUS))
        )
        proof = _proof(solution, BYTES)
        self.assertEqual(
            sorted(constraint_id(c) for c in cycle.constraints), list(proof.constraints)
        )
        self.assertEqual(("c", "w"), proof.groups)
        # The same contradiction through observations: a read of the write's
        # bytes, then a status reporting the checkout's bytes, nothing between.
        observed = make_problem(
            [checkout, write],
            [sees("read", 12, 13, P, W, BYTES), sees("status", 14, 15, P, T, STATUS)],
            initial={P: ABSENT},
        )
        solution = solver.solve(observed)
        self.assertEqual(("read", "status"), _proof(solution, BYTES).observations)
        legal = make_problem(
            [checkout, write], [sees("read", 12, 13, P, W, BYTES)], initial={P: ABSENT}
        )
        self.assertEqual(SATISFIED, _status(solver.solve(legal), BYTES))

    def test_5_2_raw_info_write(self) -> None:
        write = group("w", 1, math.inf, produce(P, W), acked=False)
        seen = make_problem([write], [sees("r", 5, 6, P, W, BYTES)], initial={P: T})
        solution = solver.solve(seen)
        self.assertEqual(SATISFIED, _status(solution, BYTES))
        assert solution.witness is not None
        self.assertIn("w", solution.witness.happened)
        unseen = make_problem([write], [sees("r", 5, 6, P, T, BYTES)], initial={P: T})
        solution = solver.solve(unseen)
        self.assertEqual(SATISFIED, _status(solution, BYTES))
        reverted = make_problem(
            [write],
            [sees("r1", 5, 6, P, W, BYTES), sees("r2", 7, 8, P, T, STATUS)],
            initial={P: T},
        )
        solution = solver.solve(reverted)
        self.assertEqual(VIOLATED, _status(solution, BYTES))
        self.assertEqual(("r1", "r2"), _proof(solution, BYTES).observations)

    def test_5_3_overlapping_checkout_attempts_with_exclusive(self) -> None:
        guard = ObligationRef("t.checkout-guard", "c1+c2")
        first = group("c1", 0, 10, produce(P, T))
        second = group("c2", 1, math.inf, produce(P, W), acked=False)
        exclusive = Exclusive(("c1", "c2"), guard)
        applied = make_problem(
            [first, second], [sees("r", 11, 12, P, T, BYTES)], [exclusive], {P: ABSENT}
        )
        self.assertEqual(SATISFIED, _status(solver.solve(applied), BYTES))
        # The rejected attempt's bytes are visible after the applied one
        # finished: only the rejected attempt's effects could explain it.
        rejected = make_problem(
            [first, second], [sees("r", 11, 12, P, W, BYTES)], [exclusive], {P: ABSENT}
        )
        solution = solver.solve(rejected)
        self.assertEqual(
            (VIOLATED, VIOLATED), (_status(solution, BYTES), _status(solution, guard))
        )
        self.assertEqual(
            (("r",), (constraint_id(exclusive),)),
            (_proof(solution, guard).observations, _proof(solution, guard).constraints),
        )
        unguarded = make_problem(
            [first, second], [sees("r", 11, 12, P, W, BYTES)], [], {P: ABSENT}
        )
        self.assertEqual(SATISFIED, _status(solver.solve(unguarded), BYTES))
        both = make_problem(
            [first, group("c2", 1, 9, produce(P, W))], [], [exclusive], {P: ABSENT}
        )
        solution = solver.solve(both)
        self.assertEqual(VIOLATED, _status(solution, guard))
        self.assertEqual(
            (constraint_id(exclusive),), _proof(solution, guard).constraints
        )

    def test_5_4_write_and_rename_atomic_transfer(self) -> None:
        write = group("w", 0, 4, produce(A, V2))
        rename = group("mv", 0, 4, transfer(A, B))
        outcomes = {}
        for after_a in (ABSENT, V1, V2):
            for after_b in (ABSENT, V1, V2):
                together = make_problem(
                    [write, rename],
                    [observe("o", 5, 6, (A, B), ((eq(after_a), eq(after_b)),), BYTES)],
                    initial={A: V1, B: ABSENT},
                )
                apart = make_problem(
                    [write, rename],
                    [
                        sees("oa", 5, 6, A, after_a, BYTES),
                        sees("ob", 5, 6, B, after_b, BYTES),
                    ],
                    initial={A: V1, B: ABSENT},
                )
                legal = _status(solver.solve(together), BYTES) == SATISFIED
                self.assertEqual(
                    legal, _status(solver.solve(apart), BYTES) == SATISFIED
                )
                self.assertEqual(legal, exhaustive.satisfiable(together))
                outcomes[(after_a, after_b)] = legal
        self.assertEqual(
            {(ABSENT, V2), (V2, V1)}, {key for key, legal in outcomes.items() if legal}
        )


class ScopedResultTest(unittest.TestCase):
    def test_violation_survives_not_evaluated_in_the_same_component(self) -> None:
        x = cell("x")
        hard_refs = (ObligationRef("t.hard", "r1"), ObligationRef("t.hard", "r2"))
        groups, reads = hard_component("h", x, 14, hard_refs)
        planted = ObligationRef("t.planted", "bad")
        bad = sees("bad", 0, 100, x, Value("file", ("never-written",)), planted)
        problem = make_problem(groups, [*reads, bad], initial={x: ABSENT})
        solution = solver.solve(problem, budgets=SMALL)
        self.assertEqual(VIOLATED, _status(solution, planted))
        self.assertEqual(("bad",), _proof(solution, planted).observations)
        for ref in hard_refs:
            self.assertEqual(
                (NOT_EVALUATED, solver.BUDGET),
                (solution.outcomes[ref].status, solution.outcomes[ref].reason),
            )
        self.assertEqual(
            solution.outcomes[planted].component,
            solution.outcomes[hard_refs[0]].component,
        )

    def test_history_account_bounds_every_solve_and_query(self) -> None:
        """Work drawn from one account stops at its limit; what is left undecided is not evaluated."""
        x = cell("x")
        hard_refs = (ObligationRef("t.hard", "r1"), ObligationRef("t.hard", "r2"))
        groups, reads = hard_component("h", x, 14, hard_refs)
        problem = make_problem(groups, reads, initial={x: ABSENT})
        account = WorkAccount(1000)
        solution = solver.solve(problem, account=account)
        for ref in hard_refs:
            self.assertEqual(
                (NOT_EVALUATED, solver.HISTORY_BUDGET),
                (solution.outcomes[ref].status, solution.outcomes[ref].reason),
            )
        self.assertEqual(0, account.left())
        verdict, _used = solver.satisfiable(problem, work=10**9, account=account)
        self.assertEqual("unknown", verdict)
        unbounded = WorkAccount(solver.HISTORY_WORK)
        decided = solver.solve(problem, account=unbounded)
        self.assertEqual(
            {ref: o.status for ref, o in solver.solve(problem).outcomes.items()},
            {ref: o.status for ref, o in decided.outcomes.items()},
        )
        self.assertGreater(unbounded.spent["work"], 0)

    def test_violation_survives_not_evaluated_in_another_component(self) -> None:
        x, y = cell("x"), cell("y")
        hard_refs = (ObligationRef("t.hard", "r1"), ObligationRef("t.hard", "r2"))
        groups, reads = hard_component("h", x, 14, hard_refs)
        planted = ObligationRef("t.planted", "y")
        contradiction = [
            sees("y1", 4, 5, y, V2, planted),
            sees("y2", 6, 7, y, V1, planted),
        ]
        problem = make_problem(
            [*groups, group("gy", 0, 10, produce(y, V2))],
            [*reads, *contradiction],
            initial={x: ABSENT, y: V1},
        )
        solution = solver.solve(problem, budgets=SMALL)
        self.assertEqual(VIOLATED, _status(solution, planted))
        self.assertEqual(("y1", "y2"), _proof(solution, planted).observations)
        self.assertNotEqual(
            solution.outcomes[planted].component,
            solution.outcomes[hard_refs[0]].component,
        )
        self.assertEqual(NOT_EVALUATED, _status(solution, hard_refs[0]))
        # With the default budget the hard component is decided: violated.
        decided = solver.solve(
            make_problem(*hard_component("h", x, 9, hard_refs), initial={x: ABSENT})
        )
        self.assertEqual(VIOLATED, _status(decided, hard_refs[0]))

    def test_explanation_budget_is_scoped(self) -> None:
        x, y = cell("x"), cell("y")
        bad = ObligationRef("t.bad", "x")
        fine = ObligationRef("t.fine", "y")
        problem = make_problem(
            [group("g", 0, 1, produce(y, V2)), group("gx", 0, 10, produce(x, V2))],
            [
                sees("x1", 4, 5, x, V2, bad),
                sees("x2", 6, 7, x, V1, bad),
                sees("y1", 2, 3, y, V2, fine),
            ],
            initial={x: V1, y: V1},
        )
        solution = solver.solve(problem, budgets=solver.Budgets(explanation_work=1))
        self.assertEqual(
            (NOT_EVALUATED, solver.EXPLANATION_BUDGET),
            (_status(solution, bad), solution.outcomes[bad].reason),
        )
        self.assertEqual(SATISFIED, _status(solution, fine))
        self.assertEqual(VIOLATED, _status(solver.solve(problem), bad))

    def test_an_undecided_trial_stops_the_component(self) -> None:
        """The first explanation trial past its budget proves nothing and is never retried."""
        x = cell("x")
        refs = (ObligationRef("t.hard", "r1"), ObligationRef("t.hard", "r2"))
        groups, reads = hard_component("h", x, 10, refs)
        extra = ObligationRef("t.extra", "any")
        redundant = observe("a-extra", 0, 300, (x,), ((ANY,),), extra)
        problem = make_problem(groups, [redundant, *reads], initial={x: ABSENT})
        budgets = solver.Budgets(decision_work=8_000_000, explanation_work=40_000)
        solution = solver.solve(problem, budgets=budgets)
        self.assertEqual(1, solution.phases["explanation"]["unknown_answers"])
        self.assertEqual((), solution.proofs)
        for ref in (*refs, extra):
            self.assertEqual(
                (NOT_EVALUATED, solver.EXPLANATION_BUDGET),
                (_status(solution, ref), solution.outcomes[ref].reason),
            )

        smaller_groups, _reads = hard_component("h", x, 7, refs)
        smaller = solver.solve(
            make_problem(smaller_groups, [redundant, *reads], initial={x: ABSENT})
        )
        exact = _proof(smaller, refs[0])
        self.assertEqual(("hr1", "hr2"), exact.observations)
        self.assertEqual(SATISFIED, _status(smaller, extra))

    def test_planted_contradictions_are_violated_within_budget(self) -> None:
        tally = {"stale": 0, "cycle": 0}
        for seed in range(4):
            for actions, keys in ((128, 4), (512, 16), (1000, 64)):
                problem = history_problem(seed, actions=actions, keys=keys, stale=True)
                solution = solver.solve(problem)
                planted = [
                    r for r, o in solution.outcomes.items() if o.status == VIOLATED
                ]
                if planted:
                    tally["stale"] += 1
                self.assertFalse(
                    any(o.status == NOT_EVALUATED for o in solution.outcomes.values())
                )
                legal = history_problem(seed, actions=actions, keys=keys)
                self.assertTrue(
                    all(
                        o.status == SATISFIED
                        for o in solver.solve(legal).outcomes.values()
                    )
                )
                # A planted precedence cycle between two acked writes.
                writes = [g for g in legal.groups if g.status.value == "acked"][:2]
                ref = ObligationRef("t.cycle", f"{seed}")
                cyclic = dataclasses.replace(
                    legal,
                    constraints=(
                        Before(writes[0].id, writes[1].id, ref),
                        Before(writes[1].id, writes[0].id, ref),
                    ),
                )
                self.assertEqual(VIOLATED, _status(solver.solve(cyclic), ref))
                tally["cycle"] += 1
        self.assertGreater(tally["stale"], 6, tally)


class ExplanationBudgetTest(unittest.TestCase):
    """Proof cores under explanation budgets."""

    S, X = cell("s"), cell("x")
    V0, NEVER = Value("file", ("0",)), Value("file", ("never",))

    def _innocent(self) -> Problem:
        move = group("g1", 0, 1, transfer(self.S, self.X))
        bad = sees("o-bad", 5, 6, self.X, self.NEVER, ObligationRef("p.read", "o-bad"))
        ok = sees("o-ok", 5, 6, self.S, ABSENT, ObligationRef("p.read", "o-ok"))
        return make_problem(
            [move], [bad, ok], initial={self.S: self.V0, self.X: self.V0}
        )

    def test_innocent_read_is_never_violated(self) -> None:
        bad, ok = ObligationRef("p.read", "o-bad"), ObligationRef("p.read", "o-ok")
        # Satisfying the innocent read needs the residual trials' budget too.
        for budget, innocent in (
            (1_000_000, SATISFIED),
            (200, NOT_EVALUATED),
            (80, NOT_EVALUATED),
            (40, NOT_EVALUATED),
        ):
            with self.subTest(explanation_work=budget):
                solution = solver.solve(
                    self._innocent(), budgets=solver.Budgets(explanation_work=budget)
                )
                self.assertEqual(
                    (VIOLATED, innocent),
                    (_status(solution, bad), _status(solution, ok)),
                )
                self.assertEqual(
                    [("o-bad",)],
                    [p.observations for p in solution.proofs],
                )
        # Too little budget to prove anything: nothing is violated.
        starved = solver.solve(
            self._innocent(), budgets=solver.Budgets(explanation_work=16)
        )
        self.assertEqual(
            {bad: solver.EXPLANATION_BUDGET, ok: solver.EXPLANATION_BUDGET},
            {r: o.reason for r, o in starved.outcomes.items()},
        )
        self.assertEqual((), starved.proofs)

    def test_budget_limited_results_are_sound(self) -> None:
        """Every proof is a minimal core; no satisfied input completes a core within a proof."""
        tally = {"proofs": 0, "limited": 0}
        for seed in range(700_000, 701_500):
            problem = random_problem(seed, max_nodes=7, keys=("a", "b"))
            solution = solver.solve(
                problem, budgets=solver.Budgets(explanation_work=seed % 7 * 24 + 8)
            )
            tally["limited"] += any(
                o.reason == solver.EXPLANATION_BUDGET
                for o in solution.outcomes.values()
            )
            cores = []
            for proof in solution.proofs:
                tally["proofs"] += 1
                core = set(proof.observations + proof.constraints)
                cores.append(core)
                self.assertFalse(exhaustive.satisfiable(problem.restrict(core)), seed)
                for item in core:
                    self.assertTrue(
                        exhaustive.satisfiable(problem.restrict(core - {item})),
                        (seed, item),
                    )
            satisfied = {
                item.id
                for item in problem.inputs()
                if all(
                    solution.outcomes[ref].status == SATISFIED
                    for ref in item.obligations
                )
            }
            self.assertTrue(exhaustive.satisfiable(problem.restrict(satisfied)), seed)
            for item in satisfied:
                for core in cores:
                    muses = exhaustive.minimal_unsat_subsets(
                        problem.restrict(core | {item})
                    )
                    self.assertFalse(any(item in m.inputs for m in muses), (seed, item))
        self.assertGreater(tally["limited"], 20, tally)

    def test_planted_read_in_22_node_history_blames_only_itself(self) -> None:
        """A vacuous Before is never violated."""
        problem, planted = planted_mixed_history(456, 12, 10, 5)
        self.assertEqual(22, len(problem.groups) + len(problem.observations))
        solution = solver.solve(problem)
        self.assertEqual(
            {planted},
            {r for r, o in solution.outcomes.items() if o.status == VIOLATED},
        )
        self.assertTrue(
            all(
                o.status == SATISFIED
                for r, o in solution.outcomes.items()
                if r != planted
            )
        )
        vacuous = ObligationRef("t.before", "nh-g3-g0")
        self.assertEqual(SATISFIED, _status(solution, vacuous))


class FactoringTest(unittest.TestCase):
    def test_shared_outcome_variable_couples_otherwise_unrelated_groups(self) -> None:
        """Removing the variable union is a false Satisfied."""
        x, y = cell("x"), cell("y")
        v0, v1 = Value("file", ("0",)), Value("file", ("1",))
        first = group("g1", 0, 1, produce(x, v1), acked=False, var="v")
        second = group("g2", 0, 1, produce(y, v1), acked=False, var="v")
        r1, r2 = ObligationRef("t.r", "o1"), ObligationRef("t.r", "o2")
        problem = make_problem(
            [first, second],
            [sees("o1", 5, 6, x, v1, r1), sees("o2", 5, 6, y, v0, r2)],
            initial={x: v0, y: v0},
        )
        for eager, monolithic in ((False, False), (True, False), (False, True)):
            solution = solver.solve(problem, eager=eager, monolithic=monolithic)
            self.assertEqual(
                (VIOLATED, VIOLATED), (_status(solution, r1), _status(solution, r2))
            )
            self.assertEqual(1, len(solution.components))
        self.assertEqual({r1, r2}, set(exhaustive.deletion_results(problem).violated))


class IncompleteDomainTest(unittest.TestCase):
    def test_unknown_initial_value_never_contradicts(self) -> None:
        x, y = cell("x"), cell("y")
        ref, other = ObligationRef("t.x", "x"), ObligationRef("t.y", "y")
        problem = make_problem(
            [group("g", 0, 1, produce(y, V2, Guard((x,), Expect(((eq(V1),),)))))],
            [
                sees("x1", 0, 1, x, V1, ref),
                sees("x2", 2, 3, x, V2, ref),
                sees("y1", 5, 6, y, V2, other),
            ],
            initial={},
        )
        solution = solver.solve(problem)
        self.assertEqual(
            (NOT_EVALUATED, solver.UNKNOWN_INITIAL),
            (_status(solution, ref), solution.outcomes[ref].reason),
        )
        self.assertEqual(NOT_EVALUATED, _status(solution, other))
        self.assertEqual([], list(solution.proofs))

    def test_candidate_cap_never_contradicts(self) -> None:
        x = cell("x")
        ref = ObligationRef("t.x", "x")
        writes = [
            group(f"w{i}", 0, 10, produce(x, Value("file", (f"{i}",))))
            for i in range(10)
        ]
        problem = make_problem(
            writes,
            [sees("r", 5, 6, x, Value("file", ("none",)), ref)],
            initial={x: ABSENT},
        )
        capped = solver.solve(problem, budgets=solver.Budgets(max_candidates=5))
        self.assertEqual(
            (NOT_EVALUATED, solver.BUDGET),
            (_status(capped, ref), capped.outcomes[ref].reason),
        )
        self.assertEqual(VIOLATED, _status(solver.solve(problem), ref))

    def test_agree_is_left_to_direct_checks(self) -> None:
        ref = ObligationRef("t.agree", "pair")
        problem = make_problem(
            [],
            [sees("o1", 0, 1, P, V1, BYTES), sees("o2", 0, 1, P, V1, BYTES)],
            [Agree("o1", "o2", "same", ref)],
            {P: V1},
        )
        solution = solver.solve(problem)
        self.assertNotIn(ref, solution.outcomes)
        self.assertEqual(SATISFIED, _status(solution, BYTES))


class FinalReaderTest(unittest.TestCase):
    """Observations past every writer of a cell see one final value."""

    def _final_reads(self, n: int, second: int, *, late: bool = False) -> Problem:
        x = cell("x")
        groups = [
            group(f"w{i:02d}", 0, 100, produce(x, Value("file", (f"v{i}",))))
            for i in range(n)
        ]
        if late:
            groups.append(group("late", 0, 300, produce(x, V1), acked=False))
        refs = (ObligationRef("t.read", "r1"), ObligationRef("t.read", "r2"))
        reads = [
            sees("r1", 200, 201, x, Value("file", ("v0",)), refs[0]),
            sees("r2", 202, 203, x, Value("file", (f"v{second}",)), refs[1]),
        ]
        return make_problem(groups, reads, initial={x: ABSENT})

    def test_disagreeing_final_reads_are_decided_without_search(self) -> None:
        """24 overlapping writers: propagation proves two later reads must agree."""
        solution = solver.solve(self._final_reads(24, 1))
        for scope in ("r1", "r2"):
            self.assertEqual(
                VIOLATED, _status(solution, ObligationRef("t.read", scope))
            )
        self.assertGreater(solution.phases["propagation"]["final_value_conflict"], 0)
        proof = _proof(solution, ObligationRef("t.read", "r1"))
        self.assertEqual(("r1", "r2"), proof.observations)

    def test_agreeing_final_reads_are_satisfied(self) -> None:
        solution = solver.solve(self._final_reads(24, 0))
        self.assertEqual(
            {SATISFIED}, {outcome.status for outcome in solution.outcomes.values()}
        )

    def test_a_writer_overlapping_a_read_leaves_it_to_the_search(self) -> None:
        """An indeterminate writer reaching past the reads can separate them."""
        problem = self._final_reads(3, 1, late=True)
        solution = solver.solve(problem)
        self.assertNotIn("final_value_conflict", solution.phases["propagation"])
        self.assertEqual(
            exhaustive.satisfiable(problem),
            all(o.status == SATISFIED for o in solution.outcomes.values()),
        )

    def test_satisfiable_decides_within_work(self) -> None:
        self.assertEqual(
            "unsat", solver.satisfiable(self._final_reads(24, 1), work=10_000)[0]
        )
        self.assertEqual(
            "sat", solver.satisfiable(self._final_reads(24, 0), work=10_000)[0]
        )
        x = cell("x")
        hard_refs = (ObligationRef("t.hard", "r1"), ObligationRef("t.hard", "r2"))
        groups, reads = hard_component("h", x, 14, hard_refs)
        verdict, used = solver.satisfiable(
            make_problem(groups, reads, initial={x: ABSENT}), work=1_000
        )
        self.assertEqual("unknown", verdict)
        self.assertGreater(used, 1_000)

    def test_satisfiable_matches_the_reference(self) -> None:
        """`solver.satisfiable` = `exhaustive.satisfiable` on generated problems.

        Random, checkout-shaped, shared-variable, and counted (terminal
        observation) problems; a small work bound may only answer `unknown`.
        """
        tally: dict[str, int] = {}
        for seed in range(3_000_000, 3_001_500):
            random_case = random_problem(seed, max_nodes=8)
            checkout = checkout_problem(seed, max_nodes=8, unknown=False)
            cases = [
                ("random", random_case),
                ("checkout", checkout),
                ("shared", shared_variable_problem(seed)),
                ("counted", counted_problem(seed, checkout)),
                ("counted", counted_problem(seed, random_case)),
            ]
            for family, problem in cases:
                if problem is None or (
                    len(problem.groups) + len(problem.observations)
                    > exhaustive.MAX_NODES
                ):
                    continue
                expected = exhaustive.satisfiable(problem)
                verdict, _used = solver.satisfiable(problem, work=10**9)
                self.assertEqual(
                    "sat" if expected else "unsat", verdict, (family, seed)
                )
                small, _used = solver.satisfiable(problem, work=20)
                self.assertIn(small, (verdict, "unknown"), (family, seed))
                key = f"{family}-{verdict}"
                tally[key] = tally.get(key, 0) + 1
        for family in ("random", "checkout", "shared", "counted"):
            self.assertGreater(tally.get(f"{family}-sat", 0), 100, (family, tally))
            self.assertGreater(tally.get(f"{family}-unsat", 0), 100, family)


class ResidualCoreTest(unittest.TestCase):
    """Inputs left after the deletion cores that still complete a core are violated."""

    def test_every_stale_read_after_a_fresh_one_is_violated(self) -> None:
        for channels in (
            ("fuse",) * 5,
            ("thrift", "fuse", "fuse"),
            ("fuse", "thrift", "thrift"),
        ):
            with self.subTest(channels=channels):
                solution = solver.solve(lost_write_problem(channels))
                self.assertEqual(
                    {VIOLATED},
                    {o.status for o in solution.outcomes.values()},
                )
                self.assertEqual(len(channels), len(solution.proofs))
                rules = {r.rule_id for r in solution.outcomes}
                self.assertEqual(
                    {"fuse.read", *(f"{c}.stale-read" for c in channels)}, rules
                )
                fresh = solution.outcomes[ObligationRef("fuse.read", "fresh")]
                self.assertEqual(len(channels), len(fresh.proofs))

    def test_every_member_of_a_core_beside_a_proof_is_violated(self) -> None:
        """Two inputs outside every deletion core complete one with a proof member.

        Trying `P - {k} + {x}` one input `x` at a time
        left both reads (seed 500694) and both order requirements (seed
        514804) satisfied although each pair is in a minimal core with a
        proof member; `P - {k}` plus every remaining input finds them.
        """
        for seed, proofs in ((500_694, 2), (514_804, 2)):
            problem = checkout_problem(seed, max_nodes=8, unknown=False)
            solution = solver.solve(problem)
            members = {
                ref
                for core in exhaustive.minimal_unsat_subsets(problem)
                for ref in core.obligations
            }
            violated = {r for r, o in solution.outcomes.items() if o.status == VIOLATED}
            self.assertEqual(members, violated, seed)
            self.assertEqual(proofs, len(solution.proofs), seed)

    def test_matches_the_reference_on_overlapping_cores(self) -> None:
        problem = lost_write_problem(("fuse",) * 3)
        solution = solver.solve(problem)
        expected = exhaustive.deletion_results(problem)
        self.assertEqual(
            {r for r, o in solution.outcomes.items() if o.status == VIOLATED},
            set(expected.violated),
        )
        self.assertEqual(
            sorted(tuple(sorted(p.observations)) for p in solution.proofs),
            sorted(core.inputs for core in expected.cores),
        )


class TerminalObservationTest(unittest.TestCase):
    """A count read after every marker writer."""

    def test_a_counted_checkout_matches_the_joint_decision(self) -> None:
        for paths in (3, 8):
            for extra in (0, 1):
                with self.subTest(paths=paths, extra=extra):
                    problem = _counted(paths, extra)
                    solution = solver.solve(problem)
                    joint = solver.solve(problem, monolithic=True)
                    self.assertEqual(
                        {r: o.status for r, o in joint.outcomes.items()},
                        {r: o.status for r, o in solution.outcomes.items()},
                    )
                    self.assertEqual(
                        VIOLATED if extra else SATISFIED, _status(solution, COUNT)
                    )
                    if not extra:
                        assert solution.witness is not None
                        self.assertTrue(explains(problem, solution.witness))

    def test_counts_outside_their_bounds_are_refuted_by_propagation(self) -> None:
        """One conflict too many exceeds what the dirty paths' markers may add;
        one too few is below what the acked groups ordered before the count
        surely mark. Propagation refutes both trials that hold the count and
        its orderings, so explaining either costs little work."""
        for extra in (1, -1):
            with self.subTest(extra=extra):
                solution = solver.solve(_counted(8, extra))
                self.assertEqual(VIOLATED, _status(solution, COUNT))
                self.assertGreater(solution.phases["propagation"]["zero_candidate"], 0)
                self.assertLess(solution.phases["explanation"]["work"], 100_000)

    def test_matches_the_reference(self) -> None:
        for ordered in (True, False):
            for extra in (-1, 0, 1):
                with self.subTest(ordered=ordered, extra=extra):
                    problem = _counted(3, extra, ordered=ordered)
                    solution = solver.solve(problem)
                    expected = exhaustive.deletion_results(problem)
                    self.assertEqual(
                        set(expected.violated),
                        {
                            r
                            for r, o in solution.outcomes.items()
                            if o.status == VIOLATED
                        },
                    )
                    self.assertEqual(
                        sorted(core.inputs for core in expected.cores),
                        sorted(
                            tuple(sorted(p.observations + p.constraints))
                            for p in solution.proofs
                        ),
                    )


class LiveValuesTest(unittest.TestCase):
    def test_projection_preserves_memo_key_equivalence(self) -> None:
        indices = {}
        for live in range(8):
            compact_to_full = {}
            full_to_compact = {}
            for values in itertools.product((ABSENT, V1, V2), repeat=3):
                full = tuple(
                    value if live >> k & 1 else None for k, value in enumerate(values)
                )
                compact = solver._live_values(values, live, indices)
                self.assertEqual(
                    full, compact_to_full.setdefault(compact, full), (live, values)
                )
                self.assertEqual(
                    compact, full_to_compact.setdefault(full, compact), (live, values)
                )
            self.assertEqual(3 ** live.bit_count(), len(compact_to_full))

    def test_only_live_values_distinguish_states(self) -> None:
        indices = {}
        original = solver._live_values((V1, ABSENT, V2), 0b101, indices)
        self.assertEqual((V1, V2), original)
        self.assertEqual(original, solver._live_values((V1, V2, V2), 0b101, indices))
        self.assertNotEqual(
            original, solver._live_values((V2, ABSENT, V2), 0b101, indices)
        )
        self.assertNotEqual(
            original, solver._live_values((V1, ABSENT, V1), 0b101, indices)
        )

    def test_empty_and_sparse_masks_share_no_value_cache(self) -> None:
        indices = {}
        values = (V1,) * 128
        self.assertEqual((), solver._live_values(values, 0, indices))
        self.assertEqual((V1,), solver._live_values(values, 1 << 127, indices))
        changed = (*values[:-1], V2)
        self.assertEqual((V2,), solver._live_values(changed, 1 << 127, indices))
        self.assertEqual((), solver._live_values(changed, 0, indices))
        self.assertEqual({0: (), 1 << 127: (127,)}, indices)


class ComponentBudgetTest(unittest.TestCase):
    def _components(self, names: list[str]) -> Problem:
        values = [Value("file", (f"v{i}",)) for i in range(4)]
        groups, reads, initial = [], [], {}
        for name, key in zip(names, "abcde"):
            target = cell(key)
            initial[target] = values[0]
            groups += [
                group(
                    f"{name}w{i}", 0, 4, produce(target, values[i % 3 + 1]), acked=False
                )
                for i in range(4)
            ]
            reads.append(
                sees(f"{name}o", 5, 6, target, values[1], ObligationRef("r.read", key))
            )
        return make_problem(groups, reads, initial=initial)

    def test_each_component_gets_a_fixed_share_whatever_its_name(self) -> None:
        """No component depends on what others left over, so renaming changes nothing."""
        names = ["A", "B", "C", "D", "E"]
        one = max(
            c["decision_work"]
            for c in solver.solve(self._components(names)).components.values()
        )
        budgets = solver.Budgets(decision_work=one * solver.COMPONENT_SHARE)
        for order in (names, ["E", "B", "C", "D", "A"]):
            solution = solver.solve(self._components(order), budgets=budgets)
            self.assertEqual(
                {SATISFIED}, {o.status for o in solution.outcomes.values()}, order
            )
        starved = solver.solve(
            self._components(names),
            budgets=solver.Budgets(decision_work=(one - 1) * solver.COMPONENT_SHARE),
        )
        self.assertEqual({NOT_EVALUATED}, {o.status for o in starved.outcomes.values()})


class ProofGroupsTest(unittest.TestCase):
    def test_many_groups_sharing_one_outcome_variable(self) -> None:
        """Proof construction is iterative: 1,500 siblings of one variable are fine."""
        x = cell("x")
        siblings = [
            group(f"g{i:04d}", 0, 1, produce(cell(f"c{i}"), V1), acked=False, var="v")
            for i in range(1500)
        ]
        writer = group("gw", 0, 1, produce(x, V2), acked=False, var="v")
        reads = [
            sees("o1", 2, 3, x, V2, ObligationRef("t.read", "o1")),
            sees("o2", 4, 5, x, V1, ObligationRef("t.read", "o2")),
        ]
        initial = {cell(f"c{i}"): ABSENT for i in range(1500)}
        initial[x] = V1
        solution = solver.solve(
            make_problem([*siblings, writer], reads, initial=initial)
        )
        (proof,) = solution.proofs
        self.assertEqual(1501, len(proof.groups))
