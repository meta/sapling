# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""The consistency engine: decides `semantics` per obligation within budgets.

Pipeline for one `Problem`:

1. Construction. Index every effect by the cell it writes (the
   interference index), name every obligation-bearing input (Agree is a
   direct check, never engine input), and compute the relevance closure:
   the cells observations and requirements read, closed under the guards
   and transfer sources of effects that write a relevant cell, and the
   groups that write a relevant cell, carry a requirement, are a
   precedence endpoint, or name an Exclusive variable. Every other cell
   and group is eliminated (lazy mode): an eliminated group writes only
   cells nothing reads and has no constraint, so it can always be placed
   at any point of its window (or dropped when indeterminate), and every
   lazy explanation extends to a full one. Initial values are queried
   only for relevant cells. An input whose relevance closure reaches a
   cell with no known initial value is not engine input: its obligations
   are not evaluated unless another input's proof violates them.
2. Factor graph. Union-find over relevant nodes joins: precedence
   endpoints, groups sharing an outcome variable, the groups of one
   Exclusive, and every reader (observation, requirement, guard, transfer
   source) with every writer of the read cell whose window lets it precede
   the reader. Footprints never join anything. `monolithic=True` joins
   everything (cross-check only).
3. Propagation per component: Exclusive over acked variables; precedence
   cycles and window infeasibility over always-happening nodes (earliest
   points along precedence edges); per-reader producer candidates (initial
   value, values of writers that may precede it, minus values a forced
   unguarded writer surely overwrites; a transfer contributes only the
   values its source can ever hold in the component) — a reader no
   candidate row can satisfy decides the component unsatisfiable without
   search, and so does a count (`effects.Count`) whose total lies outside
   what its cells must and may contribute (`_tally_readable`).
4. Finite choice without search: a component with no observation and no
   requirement is satisfied by dropping every indeterminate group. Every
   other component, however small, goes to the frontier search below.
5. Frontier search: just-in-time linearization over the component's nodes,
   memoizing failed states by (remaining nodes, values of cells a
   remaining node reads). Each sound reduction is documented where it is
   applied: dominating moves (forced placements, dead indeterminate
   groups), forward checking of every required check (a count by the
   bounds its cells' values and remaining writers allow), persistent sets
   seeded at the earliest deadlines, and restarts with new candidate
   orders on a Luby schedule that keep every failed state.

Results per obligation: a
satisfiable component satisfies its pending obligations. An unsatisfiable
one yields deletion sets over its inputs in `semantics.deletion_order`
(ID-free, latest evidence first): each candidate is tried once, deleted
when the rest stays unsatisfiable and necessary otherwise, so every set
is a minimal unsatisfiable subset and its obligations are violated with a
`ProofCertificate`; the set is removed and the component solved again.
Once the rest is satisfiable, for every proof and member, while the proof
minus that member plus every input not yet violated is unsatisfiable, its
deletion set is a new proof (`_Run._residual`); the others are
satisfied. The first explanation trial that runs out of budget stops the
component: proofs found so far stand, and every other input is not
evaluated. Budgets are per component (decision: 1/COMPONENT_SHARE of
DECISION_WORK; explanation: EXPLANATION_WORK), never what other
components left over. Without an account, every trial runs on the same
inputs at any budget until that point, so a larger budget only turns
not-evaluated into decided. With the check's history-wide account
(HISTORY_WORK), every meter also draws from it: once it is empty, what is
undecided is not evaluated with HISTORY_BUDGET, so a component's result
can then depend on what the components solved before it spent.
`eager=True` disables elimination (cross-check only).
"""

from __future__ import annotations

import json
import math
import random
from bisect import bisect_right
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

from .effects import (
    ABSENT,
    Agree,
    ANY,
    Atom,
    Cell,
    constraint_id,
    Count,
    EffectGroup,
    EffectKind,
    Exclusive,
    Expect,
    ObligationRef,
    Observation,
    ProofCertificate,
    Status,
    UnknownInitial,
    Value,
)
from .jsonio import digest
from .property import (
    ObligationStatus,
    SOLVER_HISTORY_BUDGET,
    SOLVER_UNAVAILABLE,
    WorkAccount,
)
from .semantics import (
    Assignment,
    deletion_order,
    node_contents,
    precedence,
    Problem,
    requirement_id,
)

# Deterministic logical budgets. Units are primitive engine steps (index
# insertions, candidate inspections, placement attempts, effect
# applications, mask scans). They enter the semantic digest through
# SOLVER_BUDGETS. Calibrated on checkout-like legal histories
# (`mixed_history`): 55 nodes need at most 0.24M per component, 140
# nodes need 0.6M at the median and more than 100M at worst; the limits
# are ten times the first calibration.
DECISION_WORK = 320_000_000
EXPLANATION_WORK = 80_000_000
# The search and explanation meters of every solve and membership query of
# one check also draw from this history-wide total, whatever the
# per-component limits allow. Problem construction is not metered, and
# attribution trials have their own per-proof budget.
HISTORY_WORK = 4 * DECISION_WORK
MAX_LIVE_STATES = 2_000_000
MAX_CANDIDATES = 4_096
SOLVER_BUDGETS: Mapping[str, int] = {
    "decision_work": DECISION_WORK,
    "explanation_work": EXPLANATION_WORK,
    "history_work": HISTORY_WORK,
    "max_live_states": MAX_LIVE_STATES,
    "max_candidates": MAX_CANDIDATES,
}
# Every component may spend 1/COMPONENT_SHARE of the decision work and
# the whole explanation work, whatever other components spent; one
# explanation trial may spend at most 1/TRIAL_SHARE of the explanation
# work.
COMPONENT_SHARE = 4
# A search restarts with a new candidate order after RESTART_WORK times the
# next Luby term of work, keeping every state it proved failed.
RESTART_WORK = 100_000
# Persistent sets are seeded from this many earliest-deadline required nodes.
SEEDS = 3
TRIAL_SHARE = 8

BUDGET = "core.solver.budget"
EXPLANATION_BUDGET = "core.solver.explanation-budget"
HISTORY_BUDGET = SOLVER_HISTORY_BUDGET
UNKNOWN_INITIAL = "core.solver.unknown-initial"
UNAVAILABLE = SOLVER_UNAVAILABLE

_UNKNOWN_VALUE = Value("unknown-initial")


@dataclass(frozen=True)
class Budgets:
    """The deterministic limits one solve runs under."""

    decision_work: int = DECISION_WORK
    explanation_work: int = EXPLANATION_WORK
    max_live_states: int = MAX_LIVE_STATES
    max_candidates: int = MAX_CANDIDATES


DEFAULT_BUDGETS = Budgets()


@dataclass(frozen=True)
class Outcome:
    """The engine's result for one obligation.

    `component` names the factor component that decided it; `proofs` every
    certificate that violates it, in the order the engine found them;
    `reason` the not-evaluated reason ID.
    """

    status: ObligationStatus
    component: str | None = None
    proofs: tuple[str, ...] = ()
    reason: str | None = None
    detail: str = ""


@dataclass(frozen=True)
class Solution:
    """Per-obligation outcomes, proofs, deterministic counters, and a witness.

    `proofs` are sorted by ID; each lists every obligation its
    inputs support. `witness` is a complete assignment when every
    component was satisfiable without removing any proof and no input was
    unsupported.
    """

    outcomes: Mapping[ObligationRef, Outcome]
    proofs: tuple[ProofCertificate, ...]
    phases: Mapping[str, Mapping[str, int]]
    components: Mapping[str, Mapping[str, int]]
    witness: Assignment | None = None


class _Exhausted(Exception):
    """A deterministic budget ran out."""


class _Meter:
    """Counts work into several counters and raises past `limit`.

    `phased` meters charge their phase sink (`sinks[0]`) to whichever
    phase is running; the decider switches it between propagation and
    search.
    """

    def __init__(
        self, limit: int, sinks: list[tuple[dict[str, int], str]], phased: bool = False
    ) -> None:
        self.limit = limit
        self.used = 0
        self.sinks = sinks
        self.phased = phased

    def spend(self, units: int = 1) -> None:
        self.used += units
        for sink, key in self.sinks:
            sink[key] = sink.get(key, 0) + units
        if self.used > self.limit:
            raise _Exhausted()


def _bump(counters: dict[str, int], key: str, units: int = 1) -> None:
    counters[key] = counters.get(key, 0) + units


@dataclass(frozen=True)
class _Input:
    """One obligation-bearing input as the engine sees it."""

    id: str
    kind: str  # observation | precedence | exclusive | requirement
    obligations: tuple[ObligationRef, ...]
    node: str = ""  # observation id, requirement's group id
    edge: tuple[str, str] = ("", "")
    variables: tuple[str, ...] = ()
    index: int = -1  # requirement position


_SENTINEL_WINDOW = (math.inf, -math.inf)


class _Model:
    """The interference index, inputs, and relevance closure of one problem."""

    def __init__(self, problem: Problem, eager: bool, counters: dict[str, int]):
        self.problem = problem
        self.eager = eager
        self.counters = counters
        self.groups: dict[str, EffectGroup] = {g.id: g for g in problem.groups}
        self.observations: dict[str, Observation] = {
            o.id: o for o in problem.observations
        }
        self.window: dict[str, tuple[float, float]] = {}
        for node in (*problem.groups, *problem.observations):
            self.window[node.id] = (node.window.lo, node.window.hi)
        self.var_groups: dict[str, list[str]] = {}
        for gid in sorted(self.groups):
            self.var_groups.setdefault(self.groups[gid].outcome_var, []).append(gid)
        self.acked_vars = frozenset(
            g.outcome_var for g in problem.groups if g.status == Status.ACKED
        )
        # The interference index: cell -> (group, effect) writing it.
        self.cell_writes: dict[Cell, list[tuple[str, Any]]] = {}
        for gid in sorted(self.groups):
            for effect in self.groups[gid].effects:
                for cell in effect.writes():
                    self.cell_writes.setdefault(cell, []).append((gid, effect))
                    _bump(counters, "work")
        self.inputs = self._inputs()
        _bump(counters, "work", len(self.inputs))

    def _inputs(self) -> list[_Input]:
        """Return every obligation-bearing input in `deletion_order`.

        Agree is a direct check, never engine input.
        """
        found = [
            _Input(o.id, "observation", o.obligations, node=o.id)
            for o in self.problem.observations
        ]
        for constraint in self.problem.constraints:
            if isinstance(constraint, Agree):
                continue
            name = constraint_id(constraint)
            ref = (constraint.obligation,)
            before = precedence(constraint)
            if before is not None:
                found.append(_Input(name, "precedence", ref, edge=(before.a, before.b)))
            else:
                assert isinstance(constraint, Exclusive)
                found.append(
                    _Input(name, "exclusive", ref, variables=constraint.outcome_vars)
                )
        for gid in sorted(self.groups):
            for index, requirement in enumerate(self.groups[gid].requires):
                found.append(
                    _Input(
                        requirement_id(gid, index),
                        "requirement",
                        (requirement.obligation,),
                        node=gid,
                        index=index,
                    )
                )
        keys = deletion_order(self.problem)
        return sorted(found, key=lambda item: keys[item.id])

    def input_cells(self, item: _Input) -> tuple[Cell, ...]:
        """Return the cells an input reads directly."""
        if item.kind == "observation":
            return self.observations[item.node].cells
        if item.kind == "requirement":
            return self.groups[item.node].requires[item.index].cells
        return ()

    def input_groups(self, item: _Input) -> list[str]:
        """Return the groups an input names directly."""
        if item.kind == "requirement":
            return [item.node]
        if item.kind == "precedence":
            return [node for node in item.edge if node in self.groups]
        if item.kind == "exclusive":
            return [gid for var in item.variables for gid in self.var_groups[var]]
        return []

    def closure(self, inputs: Iterable[_Input]) -> tuple[set[Cell], set[str]]:
        """Return the relevant cells and groups of `inputs` (see module doc)."""
        cells: set[Cell] = set()
        groups: set[str] = set()
        pending: list[Cell] = []

        def add(cell: Cell) -> None:
            if cell not in cells:
                cells.add(cell)
                pending.append(cell)

        for item in inputs:
            for cell in self.input_cells(item):
                add(cell)
            groups.update(self.input_groups(item))
        while pending:
            cell = pending.pop()
            for gid, effect in self.cell_writes.get(cell, ()):
                _bump(self.counters, "work")
                groups.add(gid)
                if effect.guard is not None:
                    for read in effect.guard.cells:
                        add(read)
                if effect.source is not None and effect.cell == cell:
                    add(effect.source)
        return cells, groups

    def reaching(self, unknown: set[Cell], inputs: Sequence[_Input]) -> set[str]:
        """Return the IDs of inputs whose relevance closure reaches `unknown`."""
        tainted = set(unknown)
        pending = list(unknown)
        readers: dict[Cell, list[Cell]] = {}
        for cell, writes in self.cell_writes.items():
            for _gid, effect in writes:
                if effect.guard is not None:
                    for read in effect.guard.cells:
                        readers.setdefault(read, []).append(cell)
                if effect.source is not None and effect.cell == cell:
                    readers.setdefault(effect.source, []).append(cell)
        while pending:
            for cell in readers.get(pending.pop(), ()):
                if cell not in tainted:
                    tainted.add(cell)
                    pending.append(cell)
        return {
            item.id
            for item in inputs
            if any(cell in tainted for cell in self.input_cells(item))
        }

    def group_reads(self, gid: str, relevant: set[Cell] | None) -> list[Cell]:
        """Return the cells a group reads; with `relevant`, only for relevant writes."""
        group = self.groups[gid]
        reads = [cell for requirement in group.requires for cell in requirement.cells]
        for effect in group.effects:
            if relevant is not None and not any(c in relevant for c in effect.writes()):
                continue
            if effect.guard is not None:
                reads.extend(effect.guard.cells)
            if effect.source is not None and (
                relevant is None or effect.cell in relevant
            ):
                reads.append(effect.source)
        return reads

    def can_precede(self, writer: str, reader: str) -> bool:
        """Return whether `writer` may precede `reader` in some order."""
        return not self.window[reader][1] < self.window[writer][0]

    def proof_groups(self, core: Sequence[_Input]) -> list[str]:
        """Return the groups an unsatisfiable set needs to stay so on its own.

        Starting from the groups the set names and the readers it
        contains, add every writer of a read cell that may precede the
        reader, every group sharing an added group's outcome variable (an
        acked sibling is what makes the variable happen), and treat each
        added group as a reader of everything it reads. Any group outside
        the result can be placed after every reader it could disturb, so
        the set plus these groups is unsatisfiable exactly when the set is.
        """
        groups: set[str] = set()
        readers: list[tuple[str, Cell]] = []
        added: list[str] = []
        for item in core:
            added.extend(self.input_groups(item))
            readers.extend((item.node, cell) for cell in self.input_cells(item))
        while added or readers:
            if added:
                gid = added.pop()
                if gid not in groups:
                    groups.add(gid)
                    readers.extend((gid, cell) for cell in self.group_reads(gid, None))
                    added.extend(self.var_groups[self.groups[gid].outcome_var])
                continue
            reader, cell = readers.pop()
            for gid, _effect in self.cell_writes.get(cell, ()):
                if gid != reader and self.can_precede(gid, reader):
                    added.append(gid)
        return sorted(groups)


# A compiled expectation: rows of (state index, atom); () rows = never holds.
_Rows = tuple[tuple[tuple[int, Atom], ...], ...]


def _value_key(value: Value) -> str:
    return json.dumps(value.document(), sort_keys=True)


def _live_values(
    values: Sequence[Value], live: int, indices: dict[int, tuple[int, ...]]
) -> tuple[Value, ...]:
    """Project onto live cells, caching their indices for one component."""
    positions = indices.get(live)
    if positions is None:
        positions = tuple(k for k in range(live.bit_length()) if live >> k & 1)
        indices[live] = positions
    return tuple(values[k] for k in positions)


def _holds(rows: _Rows, values: Sequence[Value]) -> bool:
    for row in rows:
        for index, atom in row:
            if not atom.matches(values[index]):
                break
        else:
            return True
    return False


@dataclass(frozen=True)
class _Tally:
    """A compiled count: (state index, low, high) per dynamic cell, and the
    low and high weight of the static cells that hold the counted value."""

    entries: tuple[tuple[int, int, int], ...]
    value: Value
    low: int
    high: int
    total: int

    def holds(self, values: Sequence[Value]) -> bool:
        low, high = self.low, self.high
        for index, lo, hi in self.entries:
            if values[index] == self.value:
                low += lo
                high += hi
        return low <= self.total <= high


@dataclass
class _Writer:
    """One group's write of one state cell, for candidate propagation."""

    node: int
    lo: float
    hi: float
    value: Value | None  # None: copies the transfer source `source`
    guarded: bool
    source: int = -1
    guard: _Rows | None = None


@dataclass
class _Local:
    """One node of a component, by local index (sorted by window)."""

    id: str
    lo: float
    hi: float
    observation: bool
    var: int  # local outcome variable, -1 for observations
    input: int  # observation input index, -1 for groups
    rows: _Rows = ()  # observation expectation
    tally: _Tally | None = None  # observation count, instead of rows
    reads: list[tuple[Cell, ...]] = field(default_factory=list)
    effects: list[tuple[int | None, Value | None, int | None, _Rows | None]] = field(
        default_factory=list
    )  # (dest, produced value, transfer source, guard)
    requires: list[tuple[int, _Rows, tuple[Cell, ...]]] = field(default_factory=list)


class _Component:
    """One factor component compiled for propagation and search."""

    def __init__(  # noqa: C901 - compiles nodes, cells, edges, and writers
        self,
        model: _Model,
        name: str,
        node_ids: list[str],
        inputs: list[_Input],
        relevant: set[Cell] | None,
        initial: Mapping[Cell, Value],
        counters: dict[str, int],
        queries: Mapping[str, Observation] | None = None,
    ) -> None:
        """Compile `node_ids` and `inputs`; `queries` replace observations by ID."""
        observations = {**model.observations, **(queries or {})}
        self.name = name
        self.inputs = inputs
        self.input_index = {item.id: i for i, item in enumerate(inputs)}
        ordered = sorted(node_ids, key=lambda n: (*model.window[n], n))
        self.index = {node: i for i, node in enumerate(ordered)}
        variables = sorted(
            {model.groups[n].outcome_var for n in ordered if n in model.groups}
        )
        self.var_names = variables
        var_index = {var: i for i, var in enumerate(variables)}
        self.var_acked = [var in model.acked_vars for var in variables]
        self.var_groups: list[list[int]] = [[] for _ in variables]
        written: list[Cell] = []
        seen: set[Cell] = set()
        for node in ordered:
            group = model.groups.get(node)
            if group is None:
                continue
            for effect in group.effects:
                for cell in effect.writes():
                    if (relevant is None or cell in relevant) and cell not in seen:
                        seen.add(cell)
                        written.append(cell)
        if relevant is None:
            for node in ordered:
                reads = (
                    observations[node].cells
                    if node in observations
                    else model.group_reads(node, None)
                )
                for cell in reads:
                    if cell not in seen:
                        seen.add(cell)
                        written.append(cell)
        self.cells = written
        self.state_index = {cell: i for i, cell in enumerate(written)}
        self.initial_values = tuple(initial[cell] for cell in written)
        self._initial = initial
        self._counters = counters
        self.nodes: list[_Local] = []
        for node in ordered:
            lo, hi = model.window[node]
            if node in observations:
                obs = observations[node]
                local = _Local(node, lo, hi, True, -1, self.input_index[node])
                if obs.expect.count is not None:
                    local.tally = self._tally(obs.cells, obs.expect.count)
                else:
                    local.rows = self._compile(obs.cells, obs.expect)
                local.reads.append(obs.cells)
            else:
                group = model.groups[node]
                local = _Local(node, lo, hi, False, var_index[group.outcome_var], -1)
                self._compile_group(local, group, relevant)
                self.var_groups[local.var].append(len(self.nodes))
            self.nodes.append(local)
        self.by_hi = sorted(range(len(self.nodes)), key=lambda i: (self.nodes[i].hi, i))
        self.edges: list[tuple[int, int, int]] = []
        self.exclusives: list[tuple[int, list[int]]] = []
        for i, item in enumerate(inputs):
            if item.kind == "precedence":
                a, b = item.edge
                self.edges.append((i, self.index[a], self.index[b]))
            elif item.kind == "exclusive":
                self.exclusives.append((i, [var_index[v] for v in item.variables]))
        self.writers: dict[int, list[_Writer]] = {}
        for i, local in enumerate(self.nodes):
            for dest, value, source, guard in local.effects:
                if dest is not None:
                    self.writers.setdefault(dest, []).append(
                        _Writer(
                            i,
                            local.lo,
                            local.hi,
                            value if source is None else None,
                            guard is not None,
                            -1 if source is None else source,
                            guard,
                        )
                    )
                if source is not None:
                    self.writers.setdefault(source, []).append(
                        _Writer(
                            i,
                            local.lo,
                            local.hi,
                            ABSENT,
                            guard is not None,
                            guard=guard,
                        )
                    )
        for writers in self.writers.values():
            writers.sort(key=lambda w: (w.lo, w.node))
        self.reach = self._reach()

    def _reach(self) -> list[frozenset[Value]]:
        """Return, per state cell, every value it can ever hold in this component.

        A cell holds its initial value, a value some effect produces into
        it, ABSENT when it is a transfer source, or a value its transfer
        sources can hold (closed to a fixpoint). Every replay stays inside
        these sets, so a transfer's copied value ranges over its source's
        set instead of over every value.
        """
        reach = [{value} for value in self.initial_values]
        copies: list[tuple[int, int]] = []
        for dest, writers in sorted(self.writers.items()):
            for w in writers:
                if w.value is not None:
                    reach[dest].add(w.value)
                else:
                    copies.append((dest, w.source))
                _bump(self._counters, "work")
        changed = True
        while changed:
            changed = False
            for dest, source in copies:
                _bump(self._counters, "work")
                if not reach[source] <= reach[dest]:
                    reach[dest] |= reach[source]
                    changed = True
        return [frozenset(values) for values in reach]

    def _compile(self, cells: tuple[Cell, ...], expect: Expect) -> _Rows:
        """Compile an expectation; static cells are folded by equivalence class."""
        dynamic = [
            (k, self.state_index[c])
            for k, c in enumerate(cells)
            if c in self.state_index
        ]
        static = [k for k, c in enumerate(cells) if c not in self.state_index]
        alive = [True] * len(expect.rows)
        classes: dict[tuple[Any, ...], int] = {}
        for k in static:
            signature = (self._initial[cells[k]], tuple(row[k] for row in expect.rows))
            classes[signature] = classes.get(signature, 0) + 1
        for (value, column), _members in classes.items():
            _bump(self._counters, "work")
            for r, atom in enumerate(column):
                if alive[r] and not atom.matches(value):
                    alive[r] = False
        _bump(self._counters, "equivalence_classes", len(classes))
        _bump(self._counters, "class_members", len(static))
        return tuple(
            tuple((index, row[k]) for k, index in dynamic if row[k] != ANY)
            for r, row in enumerate(expect.rows)
            if alive[r]
        )

    def _tally(self, cells: tuple[Cell, ...], count: Count) -> _Tally:
        """Compile a count; static cells fold into its constant weights."""
        entries, low, high = [], 0, 0
        for cell, (lo, hi) in zip(cells, count.weights):
            index = self.state_index.get(cell)
            if index is not None:
                entries.append((index, lo, hi))
            elif self._initial[cell] == count.value:
                low += lo
                high += hi
            _bump(self._counters, "work")
        return _Tally(tuple(entries), count.value, low, high, count.total)

    def _compile_group(
        self, local: _Local, group: EffectGroup, relevant: set[Cell] | None
    ) -> None:
        for effect in group.effects:
            dest = self.state_index.get(effect.cell)
            source = (
                None if effect.source is None else self.state_index.get(effect.source)
            )
            if dest is None and source is None:
                continue
            guard = (
                None
                if effect.guard is None
                else self._compile(effect.guard.cells, effect.guard.expect)
            )
            if effect.guard is not None:
                local.reads.append(effect.guard.cells)
            if effect.kind == EffectKind.PRODUCE:
                local.effects.append((dest, effect.value, None, guard))
            else:
                moved = effect.source
                assert source is not None and moved is not None, (
                    "a relevant transfer's source is relevant"
                )
                local.effects.append((dest, None, source, guard))
                local.reads.append((moved,))
        for index, requirement in enumerate(group.requires):
            name = requirement_id(group.id, index)
            if name in self.input_index:
                rows = self._compile(requirement.cells, requirement.expect)
                local.requires.append((self.input_index[name], rows, requirement.cells))
                local.reads.append(requirement.cells)

    def apply(self, local: _Local, values: tuple[Value, ...]) -> tuple[Value, ...]:
        """Return the values after a group's effects, guards read on `values`."""
        after = list(values)
        for dest, value, source, guard in local.effects:
            if guard is not None and not _holds(guard, values):
                continue
            if source is None:
                assert dest is not None and value is not None
                after[dest] = value
            else:
                if dest is not None:
                    after[dest] = values[source]
                after[source] = ABSENT
        return tuple(after)


# (cell, atom, matching writers mask, matching writers as (hi, node),
#  unguarded writers that end before the reader starts as (lo, node), by lo desc)
_Fixer = tuple[
    int, Atom, int, tuple[tuple[float, int], ...], tuple[tuple[float, int], ...]
]


class _Masks:
    """Per-node cell masks of one component, independent of the active inputs.

    `reads`/`writes` are bitmasks over state cells (observation rows,
    guards, every requirement, transfer sources; produced cells and
    transfer sources and destinations). `conflict[i]` holds every node
    that writes a cell `i` reads or writes, or reads a cell `i` writes:
    nodes that may not commute with `i`. `before[i]` holds every node
    whose window may start before `i`'s ends. Requirements of inactive
    inputs only add reads, which keeps every contract reduction sound.
    """

    def __init__(self, c: _Component) -> None:  # noqa: C901 - one pass per mask
        nodes = c.nodes
        self.reads: list[int] = []
        self.writes: list[int] = []
        self.observations = 0
        cost = 0
        for i, x in enumerate(nodes):
            reads = writes = 0
            if x.observation:
                self.observations |= 1 << i
                for row in x.rows:
                    for index, _atom in row:
                        reads |= 1 << index
                for index, _lo, _hi in x.tally.entries if x.tally else ():
                    reads |= 1 << index
            for dest, _value, source, guard in x.effects:
                if dest is not None:
                    writes |= 1 << dest
                if source is not None:
                    reads |= 1 << source
                    writes |= 1 << source
                for row in guard or ():
                    for index, _atom in row:
                        reads |= 1 << index
            for _inp, rows, _cells in x.requires:
                for row in rows:
                    for index, _atom in row:
                        reads |= 1 << index
            self.reads.append(reads)
            self.writes.append(writes)
            cost += 1
        readers = [0] * len(c.cells)
        writers = [0] * len(c.cells)
        for i in range(len(nodes)):
            for k in range(len(c.cells)):
                if self.reads[i] >> k & 1:
                    readers[k] |= 1 << i
                if self.writes[i] >> k & 1:
                    writers[k] |= 1 << i
            cost += len(c.cells)
        self.conflict: list[int] = []
        self.before: list[int] = []
        starts = [x.lo for x in nodes]
        for i, x in enumerate(nodes):
            touched = 0
            for k in range(len(c.cells)):
                if self.writes[i] >> k & 1:
                    touched |= readers[k] | writers[k]
                elif self.reads[i] >> k & 1:
                    touched |= writers[k]
            self.conflict.append(touched & ~(1 << i))
            self.before.append(((1 << bisect_right(starts, x.hi)) - 1) & ~(1 << i))
            cost += len(c.cells)
        self.fixers = [self._fixers(c, i) for i in range(len(nodes))]
        cost += sum(len(rows) for tables in self.fixers for _inp, rows in tables)
        self.tallies = [self._tally_writers(c, i) for i in range(len(nodes))]
        cost += sum(len(entries) for entries in self.tallies)
        self.readers = readers
        # check_cells[i] / check_writers[i]: every cell node i's checks read,
        # and every writer (fixer or overwriter) those checks consult; a
        # node's own bit marks a change in its requiredness.
        self.check_cells = [0] * len(nodes)
        self.check_writers = [1 << i for i in range(len(nodes))]
        for i, tables in enumerate(self.fixers):
            for _inp, rows in tables:
                for row in rows:
                    for index, _atom, mask, _fixes, overwrites in row:
                        self.check_cells[i] |= 1 << index
                        self.check_writers[i] |= mask
                        for _lo, w in overwrites:
                            self.check_writers[i] |= 1 << w
            for index, _lo, _hi, matching, other in self.tallies[i]:
                self.check_cells[i] |= 1 << index
                self.check_writers[i] |= matching | other
        self.var_nodes = [0] * len(c.var_names)
        # cell_vars[k]: outcome variables with a group writing cell k.
        self.cell_vars = [0] * len(c.cells)
        for v, members in enumerate(c.var_groups):
            for i in members:
                self.var_nodes[v] |= 1 << i
                for k in range(len(c.cells)):
                    if self.writes[i] >> k & 1:
                        self.cell_vars[k] |= 1 << v
        self._starts = starts
        ends = sorted((x.hi, i) for i, x in enumerate(nodes))
        # ends_before_start[i]: nodes whose window ends before i's starts.
        self.ends_before_start = []
        for x in nodes:
            mask = 0
            for hi, j in ends:
                if hi >= x.lo:
                    break
                mask |= 1 << j
            self.ends_before_start.append(mask)
            cost += 1
        self.cost = cost

    def starts_by(self, time: float) -> int:
        """Return the nodes whose window starts at or before `time`."""
        return (1 << bisect_right(self._starts, time)) - 1

    @staticmethod
    def _tally_writers(c: _Component, i: int) -> list[tuple[int, int, int, int, int]]:
        """Return (cell, low, high, writers that may write the counted value,
        writers that may write another) per dynamic cell of node `i`'s count,
        over writers that may precede it."""
        x = c.nodes[i]
        if x.tally is None:
            return []
        found = []
        for index, lo, hi in x.tally.entries:
            matching = other = 0
            for w in c.writers.get(index, ()):
                if w.node == i or w.lo > x.hi:
                    continue
                values = (w.value,) if w.value is not None else c.reach[w.source]
                for value in values:
                    if value == x.tally.value:
                        matching |= 1 << w.node
                    else:
                        other |= 1 << w.node
            found.append((index, lo, hi, matching, other))
        return found

    @staticmethod
    def _fixers(c: _Component, i: int) -> list[tuple[int, list[list[_Fixer]]]]:
        """Return (input, rows) per expectation node `i` must satisfy.

        One table for an observation, one per requirement of a group; each
        row entry is (cell, atom, writers that may precede `i` and can
        write a value matching the atom).
        """
        x = c.nodes[i]
        tables: list[tuple[int, _Rows]] = (
            ([] if x.tally else [(x.input, x.rows)])
            if x.observation
            else [(inp, rows) for inp, rows, _c in x.requires]
        )
        found: list[tuple[int, list[list[_Fixer]]]] = []
        for inp, rows in tables:
            table: list[list[_Fixer]] = []
            for row in rows:
                entries: list[_Fixer] = []
                for index, atom in row:
                    mask = 0
                    fixes: list[tuple[float, int]] = []
                    overwrites: list[tuple[float, int]] = []
                    for w in c.writers.get(index, ()):
                        if w.node == i or w.lo > x.hi:
                            continue
                        values = (
                            (w.value,) if w.value is not None else c.reach[w.source]
                        )
                        if any(atom.matches(v) for v in values):
                            mask |= 1 << w.node
                            fixes.append((w.hi, w.node))
                        if not w.guarded and w.hi < x.lo:
                            overwrites.append((w.lo, w.node))
                    overwrites.sort(reverse=True)
                    entries.append((index, atom, mask, tuple(fixes), tuple(overwrites)))
                table.append(entries)
            found.append((inp, table))
        return found


@dataclass
class _Decision:
    """`sat` with a witness order and happened variables, or `unsat`."""

    verdict: str
    order: tuple[str, ...] = ()
    happened: frozenset[str] = frozenset()
    searched: bool = False


class _Decider:
    """Propagation, finite choice, and frontier search over one component."""

    def __init__(
        self,
        component: _Component,
        budgets: Budgets,
        phases: dict[str, dict[str, int]],
        local: dict[str, int],
    ) -> None:
        self.c = component
        self.budgets = budgets
        self.phases = phases
        self.local = local
        self.high = 0
        # Whether a reader can be satisfied depends only on the groups and
        # the variables Exclusive drops, never on the other active inputs.
        self._readable_cache: dict[
            tuple[int, int | tuple[int, int], frozenset[int]], bool
        ] = {}
        self._static: _Masks | None = None

    def _count(self, phase: str, key: str, units: int = 1) -> None:
        _bump(self.phases[phase], key, units)
        _bump(self.local, key, units)

    def _phase(self, meter: _Meter, phase: str) -> None:
        if meter.phased:
            meter.sinks[0] = (self.phases[phase], "work")

    # Stage 3: propagation -------------------------------------------------

    def _forced_points(self, active: set[int], meter: _Meter) -> list[int] | None:
        """Return always-happening nodes in a feasible order, or None if none exists."""
        c = self.c
        forced = [
            i
            for i, n in enumerate(c.nodes)
            if (n.input in active if n.observation else c.var_acked[n.var])
        ]
        members = set(forced)
        succ: dict[int, list[int]] = {i: [] for i in forced}
        indegree = dict.fromkeys(forced, 0)
        for inp, a, b in c.edges:
            if inp in active and a in members and b in members:
                succ[a].append(b)
                indegree[b] += 1
                meter.spend()
        self._count("propagation", "forced_nodes", len(forced))
        earliest = {i: c.nodes[i].lo for i in forced}
        ready = sorted(i for i in forced if indegree[i] == 0)
        order: list[int] = []
        while ready:
            i = ready.pop(0)
            order.append(i)
            meter.spend()
            if earliest[i] > c.nodes[i].hi:
                return None
            for j in succ[i]:
                earliest[j] = max(earliest[j], earliest[i])
                indegree[j] -= 1
                if indegree[j] == 0:
                    ready.append(j)
                    ready.sort()
        if len(order) != len(forced):
            return None
        rank = {i: k for k, i in enumerate(order)}
        return sorted(order, key=lambda i: (earliest[i], rank[i]))

    def _possible(
        self,
        cell: int,
        reader: int,
        lo: float,
        hi: float,
        dropped: set[int],
        meter: _Meter,
        guarded: bool = False,
    ) -> tuple[list[Value], int]:
        """Return the values `reader` may see in `cell` and the writer count.

        With `guarded`, a writer whose guard no value it may read satisfies
        writes nothing.
        """
        c = self.c
        writers = [
            w
            for k, w in enumerate(c.writers.get(cell, ()))
            if w.node != reader
            and w.lo <= hi
            and c.nodes[w.node].var not in dropped
            and not (
                guarded
                and w.guard is not None
                and not self._readable(w.guard, w.node, (cell, k), dropped, meter)
            )
        ]
        meter.spend(1 + len(writers))
        if len(writers) > self.budgets.max_candidates:
            raise _Exhausted()
        sure = [
            w
            for w in writers
            if not w.guarded and c.var_acked[c.nodes[w.node].var] and w.hi < lo
        ]
        latest = max((s.lo for s in sure), default=-math.inf)
        values: list[Value] = []
        if not sure:
            values.append(c.initial_values[cell])
        for w in writers:
            if latest > w.hi:
                continue
            copied = (
                (w.value,)
                if w.value is not None
                else sorted(c.reach[w.source], key=_value_key)
            )
            for value in copied:
                if value not in values:
                    values.append(value)
        return values, len(writers)

    def _readable(
        self,
        rows: _Rows,
        reader: int,
        slot: int | tuple[int, int],
        dropped: set[int],
        meter: _Meter,
    ) -> bool:
        """Return whether some row can hold on values the reader may see (cached).

        `slot` names the rows: -1 for an observation, a requirement's input,
        or (cell, writer position) for the guard of a write.
        """
        key = (reader, slot, frozenset(dropped))
        meter.spend()
        if key not in self._readable_cache:
            node = self.c.nodes[reader]
            self._readable_cache[key] = self._readable_now(
                rows, reader, node.lo, node.hi, dropped, meter
            )
        return self._readable_cache[key]

    def _readable_now(
        self,
        rows: _Rows,
        reader: int,
        lo: float,
        hi: float,
        dropped: set[int],
        meter: _Meter,
    ) -> bool:
        possible: dict[int, list[Value]] = {}
        for row in rows:
            for index, _atom in row:
                if index not in possible:
                    values, count = self._possible(
                        index, reader, lo, hi, dropped, meter
                    )
                    self._count("propagation", "candidates", count)
                    possible[index] = values
        for row in rows:
            if all(
                any(atom.matches(v) for v in possible[index]) for index, atom in row
            ):
                return True
        return False

    def _propagate(  # noqa: C901 - one pass per propagation rule
        self, active: set[int], meter: _Meter
    ) -> tuple[bool, list[int] | None]:
        """Return (feasible so far, forced order) for the active inputs."""
        c = self.c
        dropped: set[int] = set()
        for inp, variables in c.exclusives:
            if inp not in active:
                continue
            acked = [v for v in variables if c.var_acked[v]]
            if len(acked) > 1:
                return False, None
            if acked:
                dropped.update(v for v in variables if v != acked[0])
        forced = self._forced_points(active, meter)
        if forced is None:
            return False, None
        for i, n in enumerate(c.nodes):
            if n.observation and n.input in active and n.tally is not None:
                if not self._tally_readable(n.tally, i, active, dropped, meter):
                    self._count("propagation", "zero_candidate")
                    return False, forced
            elif n.observation and n.input in active:
                if not self._readable(n.rows, i, -1, dropped, meter):
                    self._count("propagation", "zero_candidate")
                    return False, forced
            elif not n.observation and c.var_acked[n.var]:
                for inp, rows, _cells in n.requires:
                    if inp in active and not self._readable(
                        rows, i, inp, dropped, meter
                    ):
                        self._count("propagation", "zero_candidate")
                        return False, forced
        if self._final_value_conflict(active, dropped, meter):
            self._count("propagation", "final_value_conflict")
            return False, forced
        return True, forced

    def _tally_readable(
        self,
        tally: _Tally,
        reader: int,
        active: set[int],
        dropped: set[int],
        meter: _Meter,
    ) -> bool:
        """Return whether a count's total lies between what its cells must and
        may contribute when `reader` reads them.

        A cell may hold the counted value when one of the values it may hold
        (`_possible`, skipping writers whose guard cannot hold) is it. It
        must when that is its only value, or when an acked writer of it
        that precedes `reader` (by precedence or window) has a guard that
        surely holds and no writer may write another value.
        """
        c = self.c
        node = c.nodes[reader]
        preceding = {a for inp, a, b in c.edges if inp in active and b == reader}
        low, high = tally.low, tally.high
        for index, lo, hi in tally.entries:
            values, count = self._possible(
                index, reader, node.lo, node.hi, dropped, meter, guarded=True
            )
            self._count("propagation", "candidates", count)
            if tally.value not in values:
                continue
            high += hi
            writers = [
                w
                for w in c.writers.get(index, ())
                if w.node != reader
                and w.lo <= node.hi
                and c.nodes[w.node].var not in dropped
            ]
            meter.spend(1 + len(writers))
            if len(values) == 1 or (
                all(w.value == tally.value for w in writers)
                and any(
                    c.var_acked[c.nodes[w.node].var]
                    and (w.node in preceding or w.hi < node.lo)
                    and (
                        w.guard is None or self._surely(w.guard, w.node, dropped, meter)
                    )
                    for w in writers
                )
            ):
                low += lo
        return low <= tally.total <= high

    def _surely(
        self, rows: _Rows, reader: int, dropped: set[int], meter: _Meter
    ) -> bool:
        """Return whether some row holds on every value the reader may see."""
        node = self.c.nodes[reader]
        return any(
            all(
                all(
                    atom.matches(value)
                    for value in self._possible(
                        index, reader, node.lo, node.hi, dropped, meter
                    )[0]
                )
                for index, atom in row
            )
            for row in rows
        )

    def _final_value_conflict(
        self, active: set[int], dropped: set[int], meter: _Meter
    ) -> bool:
        """Return whether observations past every writer of a cell disagree on it.

        An observation whose window starts after every writer of a cell
        ends (`w.hi < lo`, so strictly after any point the writer takes)
        sees the cell's final value, whichever writers happened; every
        such observation sees the same one. If the values their rows allow
        for the cell (restricted to the cell's reachable values) have an
        empty intersection, no explanation exists.
        """
        c = self.c
        allowed: dict[int, set[Value]] = {}
        for n in c.nodes:
            if not (n.observation and n.input in active):
                continue
            for cell in sorted({index for row in n.rows for index, _atom in row}):
                writers = c.writers.get(cell, ())
                meter.spend(1 + len(writers))
                if any(
                    w.hi >= n.lo and c.nodes[w.node].var not in dropped for w in writers
                ):
                    continue
                values = c.reach[cell]
                found: set[Value] = set()
                for row in n.rows:
                    atom = next((a for index, a in row if index == cell), ANY)
                    found.update(v for v in values if atom.matches(v))
                meter.spend(len(values))
                known = allowed.get(cell)
                allowed[cell] = found if known is None else known & found
                if not allowed[cell]:
                    return True
        return False

    # Stage 4 and 5 ---------------------------------------------------------

    def decide(self, active: set[int], meter: _Meter) -> _Decision:
        """Decide the component restricted to `active` inputs within `meter`."""
        c = self.c
        self._phase(meter, "propagation")
        feasible, forced = self._propagate(active, meter)
        if not feasible:
            return _Decision("unsat")
        readers = any(n.observation and n.input in active for n in c.nodes) or any(
            inp in active for n in c.nodes for inp, _r, _c in n.requires
        )
        if not readers:
            assert forced is not None
            self._count("propagation", "decided")
            order = tuple(c.nodes[i].id for i in forced)
            happened = frozenset(
                c.var_names[v] for v, acked in enumerate(c.var_acked) if acked
            )
            return _Decision("sat", order, happened)
        return self._search(active, meter)

    def _masks(self, meter: _Meter) -> _Masks:
        """Return the component's static read/write/conflict masks (computed once)."""
        if self._static is None:
            self._static = _Masks(self.c)
            meter.spend(self._static.cost)
        return self._static

    def _search(  # noqa: C901 - one search loop and its placement rules
        self, active: set[int], meter: _Meter
    ) -> _Decision:
        c = self.c
        nodes = c.nodes
        n = len(nodes)
        self._phase(meter, "search")
        self._count("search", "searches")
        masks = self._masks(meter)
        obs_active = [x.observation and x.input in active for x in nodes]
        pred: list[list[int]] = [[] for _ in nodes]
        # depends[i]: nodes that may not commute with i (a shared cell
        # either writes, a sibling, a precedence, an Exclusive) or must
        # precede i in time.
        depends = [
            masks.conflict[i]
            | masks.ends_before_start[i]
            | (0 if x.observation else masks.var_nodes[x.var] & ~(1 << i))
            for i, x in enumerate(nodes)
        ]
        for inp, a, b in c.edges:
            if inp in active:
                pred[b].append(a)
                depends[a] |= 1 << b
                depends[b] |= 1 << a
        partners: list[list[int]] = [[] for _ in c.var_names]
        for inp, variables in c.exclusives:
            if inp in active:
                linked = 0
                for v in variables:
                    partners[v].extend(u for u in variables if u != v)
                    linked |= masks.var_nodes[v]
                for v in variables:
                    for i in c.var_groups[v]:
                        depends[i] |= linked & ~(1 << i)
        acked = c.var_acked
        dropped0 = 0
        for v, others in enumerate(partners):
            if acked[v]:
                for u in others:
                    dropped0 |= 1 << u
        reqs = [[rows for inp, rows, _c in x.requires if inp in active] for x in nodes]
        full = 0
        always = 0
        for i, x in enumerate(nodes):
            if obs_active[i]:
                full |= 1 << i
                always |= 1 << i
            elif not x.observation:
                full |= 1 << i
                if acked[x.var]:
                    always |= 1 << i
        unsure = [v for v in range(len(c.var_names)) if not acked[v]]
        droppable = 0
        for v in unsure:
            droppable |= 1 << v
        checked = [
            (i, [rows for inp, rows in masks.fixers[i] if inp in active])
            for i in range(n)
            if obs_active[i] or (not nodes[i].observation and reqs[i])
        ]
        tallied = [i for i in range(n) if obs_active[i] and nodes[i].tally]
        State = tuple[int, int, int, tuple[Value, ...]]
        summaries: dict[tuple[int, int], tuple[int, int, float, int]] = {}

        def summary(placed: int, dropped: int) -> tuple[int, int, float, int]:
            """Return (remaining nodes, live cells, frontier, required nodes).

            Required nodes must happen: observations, acked groups, and
            groups of a variable with a placed group. The frontier is the
            earliest deadline among them.
            """
            key = (placed, dropped)
            meter.spend()
            found = summaries.get(key)
            if found is not None:
                return found
            gone = placed
            required = always
            for v in unsure:
                if dropped >> v & 1:
                    gone |= masks.var_nodes[v]
                elif masks.var_nodes[v] & placed:
                    required |= masks.var_nodes[v]
            remaining = full & ~gone
            required &= remaining
            live = 0
            for k, readers in enumerate(masks.readers):
                if readers & remaining:
                    live |= 1 << k
            # Nodes are indexed by window start and hi >= lo, so the
            # earliest deadline is found before any start exceeds it.
            frontier = math.inf
            steps = len(masks.readers)
            rest = required
            while rest:
                low = rest & -rest
                i = low.bit_length() - 1
                rest ^= low
                steps += 1
                if nodes[i].lo > frontier:
                    break
                frontier = min(frontier, nodes[i].hi)
            meter.spend(steps)
            found = (remaining, live, frontier, required)
            summaries[key] = found
            return found

        def gone(i: int, placed: int, dropped: int) -> bool:
            if placed >> i & 1:
                return True
            x = nodes[i]
            if x.observation:
                return not obs_active[i]
            return bool(dropped >> x.var & 1)

        def mandatory(i: int, committed: int) -> bool:
            x = nodes[i]
            return x.observation or acked[x.var] or bool(committed >> x.var & 1)

        def place(
            i: int, placed: int, dropped: int, committed: int, values: tuple[Value, ...]
        ) -> tuple[int, int, int, tuple[Value, ...], int] | None:
            meter.spend()
            if gone(i, placed, dropped):
                return None
            x = nodes[i]
            own = -1 if x.observation else x.var
            if own >= 0 and any(committed >> u & 1 for u in partners[own]):
                return None
            drops = 0
            for y in c.by_hi:
                if nodes[y].hi >= x.lo:
                    break
                if y == i or gone(y, placed, dropped):
                    continue
                meter.spend()
                if mandatory(y, committed) or nodes[y].var == own:
                    return None
                drops |= 1 << nodes[y].var
            for y in pred[i]:
                if gone(y, placed, dropped) or (
                    not nodes[y].observation and drops >> nodes[y].var & 1
                ):
                    continue
                if mandatory(y, committed) or nodes[y].var == own:
                    return None
                drops |= 1 << nodes[y].var
            if x.observation:
                if not (x.tally.holds(values) if x.tally else _holds(x.rows, values)):
                    return None
                return placed | 1 << i, dropped | drops, committed, values, drops
            for rows in reqs[i]:
                if not _holds(rows, values):
                    return None
            for u in partners[own]:
                drops |= 1 << u
            meter.spend(len(x.effects))
            return (
                placed | 1 << i,
                dropped | drops,
                committed | 1 << own,
                c.apply(x, values),
                drops,
            )

        def forced(i: int, remaining: int, live: int, required: int) -> bool:
            """Whether placing `i` now loses no completion, if it drops nothing.

            True for a placeable observation, and for a group that must
            happen when no remaining node that may precede it touches a
            cell it touches, or when it carries no requirement and writes
            only cells no remaining node reads: moving such a node to the
            front of any completion keeps every check.
            """
            x = nodes[i]
            if x.observation:
                return True
            if not required >> i & 1:
                return False
            if not masks.writes[i] & live and not reqs[i]:
                return True
            return not masks.conflict[i] & masks.before[i] & remaining

        def settle(state: State, trail: list[int]) -> State:
            """Apply every dominating move until none applies.

            Forced placements (see `forced`) that drop nothing, and drops
            of an undecided outcome variable none of whose groups is
            placed and whose groups write only cells no remaining node
            reads: removing such groups from any completion changes no
            value a remaining check reads and only relaxes constraints.
            """
            placed, dropped, committed, values = state
            progress = True
            while progress:
                progress = False
                remaining, live, frontier, required = summary(placed, dropped)
                written = 0
                for k, writers in enumerate(masks.cell_vars):
                    if live >> k & 1:
                        written |= writers
                meter.spend(len(masks.cell_vars))
                dead = droppable & ~written & ~dropped & ~committed
                if dead:
                    dropped |= dead
                    progress = True
                    self._count("search", "dead_drops")
                    continue
                rest = remaining
                while rest:
                    low = rest & -rest
                    i = low.bit_length() - 1
                    rest ^= low
                    if nodes[i].lo > frontier:
                        break
                    meter.spend()
                    if not forced(i, remaining, live, required):
                        continue
                    moved = place(i, placed, dropped, committed, values)
                    if moved is not None and moved[4] == 0:
                        placed, dropped, committed, values = moved[:4]
                        trail.append(i)
                        progress = True
                        break
            return placed, dropped, committed, values

        def doomed(state: State, parent: State | None = None) -> bool:
            """Whether a required check can no longer hold.

            An observation, or a requirement of a group that must happen,
            needs some row whose every cell either keeps a matching value
            or has a remaining writer that may precede the reader and can
            write a matching value, not followed by a sure overwrite: a
            remaining unguarded writer that must happen and ends before
            the reader starts. Given the `parent` state (which passed),
            only checks whose cells, writers, or own requiredness changed
            are evaluated again.
            """
            placed, dropped, _committed, values = state
            remaining, _live, _frontier, required = summary(placed, dropped)
            changed_cells = changed_nodes = -1
            if parent is not None:
                before, _l, _f, was = summary(parent[0], parent[1])
                changed_nodes = (before & ~remaining) | (required & ~was)
                changed_cells = 0
                for k, (old, new) in enumerate(zip(parent[3], values)):
                    if old is not new and old != new:
                        changed_cells |= 1 << k
                meter.spend()

            def possible(entry: _Fixer) -> bool:
                index, atom, mask, fixes, overwrites = entry
                latest = next((lo for lo, w in overwrites if required >> w & 1), None)
                if latest is None:
                    return atom.matches(values[index]) or bool(mask & remaining)
                meter.spend()
                return any(hi >= latest and remaining >> w & 1 for hi, w in fixes)

            for i, tables in checked:
                if not required >> i & 1:
                    continue
                if not (
                    masks.check_cells[i] & changed_cells
                    or masks.check_writers[i] & changed_nodes
                ):
                    continue
                for rows in tables:
                    meter.spend()
                    if not any(all(possible(entry) for entry in row) for row in rows):
                        self._count("search", "pruned")
                        return True
            for i in tallied:
                if not required >> i & 1 or not (
                    masks.check_cells[i] & changed_cells
                    or masks.check_writers[i] & changed_nodes
                ):
                    continue
                tally = nodes[i].tally
                assert tally is not None
                low, high = tally.low, tally.high
                for index, lo, hi, matching, other in masks.tallies[i]:
                    if values[index] == tally.value:
                        high += hi
                        if not other & remaining:
                            low += lo
                    elif matching & remaining:
                        high += hi
                meter.spend(1 + len(masks.tallies[i]))
                if not low <= tally.total <= high:
                    self._count("search", "pruned")
                    return True
            return False

        live_indices: dict[int, tuple[int, ...]] = {}

        def key(state: State) -> Any:
            """Everything a state's completions depend on.

            The remaining nodes fix which variables are committed (a
            remaining group's variable is committed exactly when a sibling
            is placed, and then the sibling is not remaining) and which
            Exclusive partners are gone; cells no remaining node reads
            never matter again. The live mask is fixed by the remaining
            nodes, so dead-cell placeholders do not distinguish states.
            """
            placed, dropped, _committed, values = state
            remaining, live, _frontier, _required = summary(placed, dropped)
            return remaining, _live_values(values, live, live_indices)

        rank: list[int] | None = None

        def candidates(state: State) -> list[int]:
            """Return a persistent set of next placements.

            A required node f precedes every node starting after its
            deadline, so only nodes starting by then (its pool) can come
            before it. Close {f} under `depends` within the pool: the first
            closure member of any completion commutes ahead of the pool
            nodes before it, so some completion starts inside the closure.
            The smallest closure over the SEEDS earliest-deadline required
            nodes is used.
            """
            remaining, _live, _frontier, required = summary(state[0], state[1])
            seeds = sorted((nodes[i].hi, i) for i in range(n) if required >> i & 1)
            closure = remaining
            for deadline, seed in seeds[:SEEDS]:
                pool = remaining & masks.starts_by(deadline)
                grown = grow = 1 << seed
                while grow:
                    meter.spend()
                    bit = grow & -grow
                    grow ^= bit
                    new = depends[bit.bit_length() - 1] & pool & ~grown
                    grown |= new
                    grow |= new
                if grown.bit_count() < closure.bit_count():
                    closure = grown
            found = []
            rest = closure
            while rest:
                low = rest & -rest
                rest ^= low
                found.append(low.bit_length() - 1)
            meter.spend(len(found))
            if rank is not None:
                found.sort(key=rank.__getitem__)
            return found

        def finished(state: State) -> bool:
            return not summary(state[0], state[1])[3]

        path: list[int] = []

        def run(root: State, failed: set[Any], limit: int) -> bool | None:
            """One depth-first pass from `root`; None when it passes `limit` work."""
            stack: list[list[Any]] = [[root, candidates(root), 0, len(path)]]
            while stack:
                if meter.used > limit:
                    return None
                frame = stack[-1]
                state, options, pos, base = frame
                pushed = False
                while pos < len(options):
                    i = options[pos]
                    pos += 1
                    moved = place(i, *state)
                    if moved is None:
                        continue
                    path.append(i)
                    found = settle(moved[:4], path)
                    if finished(found):
                        return True
                    seen = key(found)
                    if seen in failed:
                        self._count("search", "cache_hits")
                        del path[base:]
                        continue
                    if doomed(found, state):
                        failed.add(seen)
                        del path[base:]
                        continue
                    self._count("search", "branches")
                    frame[2] = pos
                    stack.append([found, candidates(found), 0, len(path)])
                    pushed = True
                    break
                if not pushed:
                    failed.add(key(state))
                    self.high = max(self.high, len(failed))
                    if len(failed) > self.budgets.max_live_states:
                        raise _Exhausted()
                    stack.pop()
                    if stack:
                        del path[stack[-1][3] :]
            return False

        root = settle((0, dropped0, 0, c.initial_values), path)
        if not finished(root) and doomed(root):
            return _Decision("unsat", searched=True)
        if not finished(root):
            # States proven to have no completion survive restarts: a
            # state met again off the current path is never an ancestor
            # (remaining nodes strictly shrink along a path), so it was
            # fully explored. Restarts reorder candidates and let each
            # pass run for the next Luby term times RESTART_WORK.
            failed: set[Any] = set()
            kept = len(path)
            restart = 0
            while True:
                limit = meter.used + _luby(restart + 1) * RESTART_WORK
                outcome = run(root, failed, limit)
                if outcome is False:
                    return _Decision("unsat", searched=True)
                if outcome:
                    break
                del path[kept:]
                restart += 1
                self._count("search", "restarts")
                rank = _permutation(n, restart)
        committed = 0
        for i in path:
            if not nodes[i].observation:
                committed |= 1 << nodes[i].var
        happened = frozenset(
            c.var_names[v]
            for v in range(len(c.var_names))
            if committed >> v & 1 or acked[v]
        )
        return _Decision("sat", tuple(nodes[i].id for i in path), happened, True)


def _luby(index: int) -> int:
    """Return the `index`-th (1-based) term of the Luby sequence 1,1,2,1,1,2,4,..."""
    while True:
        k = 1
        while (1 << k) - 1 < index:
            k += 1
        if (1 << k) - 1 == index:
            return 1 << (k - 1)
        index -= (1 << (k - 1)) - 1


def _permutation(n: int, seed: int) -> list[int]:
    """Return a deterministic rank of `n` nodes for restart `seed` (no hash randomization)."""
    order = list(range(n))
    random.Random(seed).shuffle(order)
    rank = [0] * n
    for position, node in enumerate(order):
        rank[node] = position
    return rank


class _UnionFind:
    def __init__(self, items: Iterable[str]) -> None:
        self.parent = {item: item for item in items}

    def find(self, item: str) -> str:
        parent = self.parent
        while parent[item] != item:
            parent[item] = parent[parent[item]]
            item = parent[item]
        return item

    def union(self, a: str, b: str) -> None:
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            low, high = sorted((ra, rb))
            self.parent[high] = low


@dataclass
class _Factor:
    """One factor component: its nodes and inputs."""

    name: str
    nodes: list[str]
    inputs: list[_Input]
    incidences: int


def _components(  # noqa: C901 - one pass per coupling kind
    model: _Model,
    inputs: list[_Input],
    nodes: list[str],
    relevant: set[Cell] | None,
    monolithic: bool,
    counters: dict[str, int],
) -> list[_Factor]:
    """Return the factor components.

    Components are sorted by size, then by the ID-free contents of their
    nodes (`semantics.node_contents`); the name breaks ties only between
    components of identical content.
    """
    contents = node_contents(model.problem)
    uf = _UnionFind(nodes)
    present = set(nodes)
    groups = [n for n in nodes if n in model.groups]
    by_var: dict[str, str] = {}
    for gid in groups:
        var = model.groups[gid].outcome_var
        if var in by_var:
            uf.union(by_var[var], gid)
        else:
            by_var[var] = gid
    writers: dict[Cell, list[tuple[float, str]]] = {}
    for gid in groups:
        for effect in model.groups[gid].effects:
            for cell in effect.writes():
                if relevant is None or cell in relevant:
                    writers.setdefault(cell, []).append((model.window[gid][0], gid))
    reads: dict[str, Sequence[Cell]] = {
        node: (
            model.observations[node].cells
            if node in model.observations
            else model.group_reads(node, relevant)
        )
        for node in nodes
    }
    for item in inputs:
        named = [n for n in (*item.edge, item.node) if n in present]
        named += model.input_groups(item)
        for other in named[1:]:
            uf.union(named[0], other)
    starts: dict[Cell, list[float]] = {}
    chained: dict[Cell, int] = {}
    for cell, listed in writers.items():
        listed.sort()
        starts[cell] = [lo for lo, _gid in listed]
    incidences: dict[str, int] = {}
    for reader in nodes:
        for cell in reads[reader]:
            listed = writers.get(cell)
            if not listed:
                continue
            count = bisect_right(starts[cell], model.window[reader][1])
            _bump(counters, "work")
            if count == 0:
                continue
            done = chained.get(cell, 1)
            for k in range(done, count):
                uf.union(listed[k - 1][1], listed[k][1])
            chained[cell] = max(done, count)
            uf.union(reader, listed[0][1])
            incidences[reader] = incidences.get(reader, 0) + count
    if monolithic and nodes:
        for node in nodes[1:]:
            uf.union(nodes[0], node)
    members: dict[str, list[str]] = {}
    for node in sorted(nodes):
        members.setdefault(uf.find(node), []).append(node)
    owned: dict[str, list[_Input]] = {root: [] for root in members}
    for item in inputs:
        anchor = item.node or next(
            (n for n in (*item.edge, *model.input_groups(item)) if n in present), ""
        )
        owned[uf.find(anchor)].append(item)

    def key(listed: list[str]) -> tuple[int, list[str], str]:
        return len(listed), sorted(contents[n] for n in listed), listed[0]

    found = []
    for root, listed in members.items():
        total = sum(incidences.get(n, 0) for n in listed)
        _bump(counters, "couplings", total)
        factor = _Factor(f"component:{listed[0]}", listed, owned[root], total)
        found.append((key(listed), factor))
    _bump(counters, "components", len(found))
    found.sort(key=lambda item: item[0])
    return [factor for _key, factor in found]


def _proof(model: _Model, core: list[_Input]) -> ProofCertificate:
    """Certify a minimal set: every member is necessary and names its obligations."""
    observations = sorted(i.id for i in core if i.kind == "observation")
    constraints = sorted(i.id for i in core if i.kind != "observation")
    groups = model.proof_groups(core)
    document = {
        "observations": observations,
        "groups": groups,
        "constraints": constraints,
    }
    proof_digest = digest(document).removeprefix("sha256:")
    refs = sorted(
        {ref for i in core for ref in i.obligations}, key=ObligationRef.document
    )
    return ProofCertificate(
        id=f"proof:{proof_digest[:24]}",
        obligations=tuple(refs),
        observations=tuple(observations),
        groups=tuple(groups),
        constraints=tuple(constraints),
    )


class _Undecided(Exception):
    """An explanation trial ran out of budget; the component's explanation stops."""


class _Run:
    """One solve: budgets, counters, and per-input results."""

    def __init__(
        self,
        problem: Problem,
        budgets: Budgets,
        eager: bool,
        monolithic: bool,
        account: WorkAccount | None = None,
    ):
        self.problem = problem
        self.budgets = budgets
        self.eager = eager
        self.monolithic = monolithic
        self.account = account
        self.phases: dict[str, dict[str, int]] = {
            phase: {"work": 0}
            for phase in ("construction", "propagation", "search", "explanation")
        }
        self.components: dict[str, dict[str, int]] = {}
        self.proofs: list[tuple[ProofCertificate, str]] = []
        # obligation -> (proof ids, not-evaluated (component, reason), satisfied components)
        self.violated: dict[ObligationRef, list[str]] = {}
        self.unevaluated: dict[ObligationRef, list[tuple[str, str]]] = {}
        self.satisfied: dict[ObligationRef, list[str]] = {}
        self.proof_component: dict[str, str] = {}
        self.witnesses: list[tuple[_Component, _Decision]] = []
        self.clean = True

    def _meter(
        self, limit: int, sinks: list[tuple[dict[str, int], str]], phased: bool = False
    ) -> _Meter:
        """Return a meter for `limit` work that also draws from the history account."""
        if self.account is None:
            return _Meter(limit, sinks, phased)
        return _Meter(
            min(limit, self.account.left()),
            [*sinks, (self.account.spent, "work")],
            phased,
        )

    def _shortfall(self, reason: str) -> str:
        """Name the exhausted budget: the history account once it is empty, else `reason`."""
        if self.account is not None and self.account.left() == 0:
            return HISTORY_BUDGET
        return reason

    def _mark(
        self, store: dict[ObligationRef, list[Any]], items: Iterable[_Input], value: Any
    ) -> None:
        for item in items:
            for ref in item.obligations:
                store.setdefault(ref, []).append(value)

    def _supported(
        self, model: _Model
    ) -> tuple[list[_Input], dict[str, str], set[Cell], set[str], dict[Cell, Value]]:
        """Return the inputs the engine decides, the unsupported ones, and the closure.

        An input whose relevance closure reaches a cell with no known
        initial value is unsupported, and so is a precedence naming an
        observation that is.
        """
        unsupported: dict[str, str] = {}
        supported = list(model.inputs)
        relevant, groups = model.closure(supported)
        initial, unknown = self._initial(model, relevant)
        if unknown & relevant:
            tainted = model.reaching(unknown & relevant, supported)
            removed = {i for i in tainted if i in model.observations}
            for item in supported:
                if item.id in tainted or (
                    item.kind == "precedence" and removed.intersection(item.edge)
                ):
                    unsupported[item.id] = UNKNOWN_INITIAL
            supported = [i for i in supported if i.id not in unsupported]
            relevant, groups = model.closure(supported)
        return supported, unsupported, relevant, groups, initial

    def satisfiable(self, work: int) -> tuple[str, int]:
        """Decide the whole problem within `work` units: `sat`, `unsat`, or `unknown`.

        A problem with an input the engine cannot decide (`run` leaves it
        not evaluated) is `unknown`. Returns the verdict and the work spent
        deciding (construction is not metered).
        """
        construction = self.phases["construction"]
        model = _Model(self.problem, False, construction)
        supported, unsupported, relevant, groups, initial = self._supported(model)
        if unsupported:
            return "unknown", 0
        observed = {i.node for i in supported if i.kind == "observation"}
        components = _components(
            model, supported, sorted(groups | observed), relevant, False, construction
        )
        meter = self._meter(max(work, 0), [(self.phases["explanation"], "work")])
        try:
            for factor in components:
                _component, decider = self._compile(
                    model, factor, relevant, initial, {}
                )
                verdict = decider.decide(set(range(len(factor.inputs))), meter).verdict
                if verdict != "sat":
                    return verdict, meter.used
        except _Exhausted:
            return "unknown", meter.used
        return "sat", meter.used

    def run(self) -> Solution:
        construction = self.phases["construction"]
        model = _Model(self.problem, self.eager, construction)
        supported, unsupported, relevant, groups, initial = self._supported(model)
        for item in model.inputs:
            if item.id in unsupported:
                self._mark(self.unevaluated, [item], ("", unsupported[item.id]))
        if self.eager:
            groups = set(model.groups)
        _bump(construction, "groups", len(model.groups))
        _bump(construction, "observations", len(model.observations))
        _bump(construction, "inputs", len(model.inputs))
        _bump(construction, "relevant_cells", len(relevant))
        _bump(construction, "relevant_groups", len(groups))
        _bump(construction, "eliminated_groups", len(model.groups) - len(groups))
        observed = {i.node for i in supported if i.kind == "observation"}
        nodes = sorted(groups | observed)
        scope = None if self.eager else relevant
        components = _components(
            model, supported, nodes, scope, self.monolithic, construction
        )
        for factor in components:
            local: dict[str, int] = {}
            self.components[factor.name] = local
            component, decider = self._compile(model, factor, scope, initial, local)
            self._solve(model, component, decider, factor, local)
        return self._solution(model, unsupported)

    def _compile(
        self,
        model: _Model,
        factor: _Factor,
        scope: set[Cell] | None,
        initial: Mapping[Cell, Value],
        local: dict[str, int],
    ) -> tuple[_Component, _Decider]:
        """Compile a factor component and its decider."""
        component = _Component(
            model,
            factor.name,
            factor.nodes,
            factor.inputs,
            scope,
            initial,
            self.phases["construction"],
        )
        return component, _Decider(component, self.budgets, self.phases, local)

    def _initial(
        self, model: _Model, relevant: set[Cell]
    ) -> tuple[dict[Cell, Value], set[Cell]]:
        """Query initial values of relevant cells (every mentioned cell when eager)."""
        cells = set(relevant)
        if self.eager:
            cells.update(model.cell_writes)
            for o in model.observations.values():
                cells.update(o.cells)
            for gid in model.groups:
                cells.update(model.group_reads(gid, None))
        initial: dict[Cell, Value] = {}
        unknown: set[Cell] = set()
        for cell in sorted(cells, key=Cell.document):
            _bump(self.phases["construction"], "initial_queries")
            try:
                initial[cell] = self.problem.initial(cell)
            except UnknownInitial:
                initial[cell] = _UNKNOWN_VALUE
                unknown.add(cell)
        _bump(self.phases["construction"], "unknown_initial", len(unknown & relevant))
        return initial, unknown

    def _solve(
        self,
        model: _Model,
        component: _Component,
        decider: _Decider,
        factor: _Factor,
        local: dict[str, int],
    ) -> None:
        local.update(
            nodes=len(component.nodes),
            groups=sum(not n.observation for n in component.nodes),
            observations=sum(n.observation for n in component.nodes),
            cells=len(component.cells),
            variables=len(component.var_names),
            factors=len(component.inputs),
            incidences=factor.incidences,
            candidates=0,
            branches=0,
            cache_hits=0,
            states_high_water=0,
            decision_work=0,
            explanation_work=0,
            proofs=0,
        )
        meter = self._meter(
            self.budgets.decision_work // COMPONENT_SHARE,
            [(self.phases["propagation"], "work"), (local, "decision_work")],
            phased=True,
        )
        inputs = component.inputs
        active = set(range(len(inputs)))
        cores: list[list[int]] = []
        violated: set[int] = set()
        try:
            while True:
                try:
                    decision = decider.decide(active, meter)
                except _Exhausted:
                    self.clean = False
                    self._mark(
                        self.unevaluated,
                        [inputs[i] for i in sorted(active)],
                        (component.name, self._shortfall(BUDGET)),
                    )
                    return
                if decision.verdict == "sat":
                    if not cores:
                        self.witnesses.append((component, decision))
                    break
                self.clean = False
                core = self._delete(decider, sorted(active), local)
                cores.append(core)
                self._violate(model, component, local, core, violated)
                active -= set(core)
            self._residual(model, decider, component, local, cores, violated)
            self._mark(
                self.satisfied,
                [inputs[i] for i in sorted(active - violated)],
                component.name,
            )
        except _Undecided:
            self._mark(
                self.unevaluated,
                [inputs[i] for i in sorted(active - violated)],
                (component.name, self._shortfall(EXPLANATION_BUDGET)),
            )
        finally:
            local["states_high_water"] = decider.high

    def _violate(
        self,
        model: _Model,
        component: _Component,
        local: dict[str, int],
        core: list[int],
        violated: set[int],
    ) -> None:
        members = [component.inputs[i] for i in core]
        proof = _proof(model, members)
        self.proofs.append((proof, component.name))
        self.proof_component[proof.id] = component.name
        local["proofs"] += 1
        self._mark(self.violated, members, proof.id)
        violated.update(core)

    def _residual(
        self,
        model: _Model,
        decider: _Decider,
        component: _Component,
        local: dict[str, int],
        cores: list[list[int]],
        violated: set[int],
    ) -> None:
        """Violate every remaining input in a minimal set within a proof plus the rest.

        For each proof `P` in order (the deletion sets, then every proof
        this finds) and each member `k` (deletion order): while
        `P - {k}` plus every input not yet violated is unsatisfiable, its
        deletion set is a new proof. `P - {k}` is satisfiable because `P`
        is minimal, so each such set holds an input not yet violated, and
        the loop ends. Afterwards every satisfied input is jointly
        satisfiable with `P - {k}` and all other satisfied inputs, for
        every proof `P` and member `k`: it is in no minimal unsatisfiable set contained
        in such a set.
        """
        proofs = list(cores)
        everything = set(range(len(component.inputs)))
        index = 0
        while index < len(proofs):
            proof = proofs[index]
            index += 1
            for k in proof:
                while True:
                    trial = sorted((set(proof) - {k}) | (everything - violated))
                    if self._trial_unsat(decider, set(trial), local) != "unsat":
                        break
                    core = self._delete(decider, trial, local)
                    proofs.append(core)
                    self._violate(model, component, local, core, violated)

    def _trial_unsat(
        self, decider: _Decider, trial: set[int], local: dict[str, int]
    ) -> str:
        """Decide one explanation trial: sat or unsat; past its budget raise `_Undecided`.

        A trial may spend 1/TRIAL_SHARE of the explanation work, and the
        component's trials together the whole of it.
        """
        remaining = self.budgets.explanation_work - local["explanation_work"]
        meter = self._meter(
            max(0, min(self.budgets.explanation_work // TRIAL_SHARE, remaining)),
            [(self.phases["explanation"], "work"), (local, "explanation_work")],
        )
        _bump(self.phases["explanation"], "trial_checks")
        try:
            return decider.decide(trial, meter).verdict
        except _Exhausted:
            _bump(self.phases["explanation"], "unknown_answers")
            raise _Undecided() from None

    def _delete(
        self, decider: _Decider, pool: list[int], local: dict[str, int]
    ) -> list[int]:
        """Return the deletion set of the unsatisfiable `pool` (deletion order).

        Each candidate is tried once, in order: one whose trial is
        unsatisfiable is deleted, one whose trial is satisfiable is
        necessary (removing inputs never removes an explanation, so it
        stays necessary as the set shrinks). The result is a minimal
        unsatisfiable set. A trial that runs out of budget raises
        `_Undecided`: it is never retried and never picks the set.
        """
        core = list(pool)
        for candidate in pool:
            trial = set(core) - {candidate}
            if self._trial_unsat(decider, trial, local) == "unsat":
                core.remove(candidate)
        _bump(self.phases["explanation"], "core_inputs", len(core))
        return core

    def _solution(self, model: _Model, unsupported: Mapping[str, str]) -> Solution:
        outcomes: dict[ObligationRef, Outcome] = {}
        every = {ref for item in model.inputs for ref in item.obligations}
        for ref in sorted(every, key=ObligationRef.document):
            proofs = tuple(dict.fromkeys(self.violated.get(ref, ())))
            if proofs:
                outcomes[ref] = Outcome(
                    ObligationStatus.VIOLATED, self.proof_component[proofs[0]], proofs
                )
                continue
            missing = self.unevaluated.get(ref)
            if missing:
                component, reason = min(missing)
                outcomes[ref] = Outcome(
                    ObligationStatus.NOT_EVALUATED, component or None, reason=reason
                )
                continue
            outcomes[ref] = Outcome(
                ObligationStatus.SATISFIED, min(self.satisfied[ref])
            )
        self.phases["explanation"]["proofs"] = len(self.proofs)
        proofs = tuple(sorted((p for p, _c in self.proofs), key=lambda p: p.id))
        witness = None
        if self.clean and not unsupported:
            witness = self._witness(model)
        return Solution(
            outcomes,
            proofs,
            {k: dict(sorted(v.items())) for k, v in self.phases.items()},
            {k: dict(sorted(v.items())) for k, v in sorted(self.components.items())},
            witness,
        )

    def _witness(self, model: _Model) -> Assignment:
        """Merge component explanations and place eliminated groups at their start."""
        keyed: list[tuple[float, int, int, str]] = []
        happened: set[str] = set()
        decided: set[str] = set()
        placed: set[str] = set()
        for rank, (component, decision) in enumerate(self.witnesses):
            happened.update(decision.happened)
            decided.update(component.var_names)
            point = -math.inf
            for position, node in enumerate(decision.order):
                point = max(point, model.window[node][0])
                keyed.append((point, rank, position, node))
                placed.add(node)
        for gid in sorted(model.groups):
            var = model.groups[gid].outcome_var
            if var not in decided and var in model.acked_vars:
                happened.add(var)
        for gid in sorted(model.groups):
            if gid not in placed and model.groups[gid].outcome_var in happened:
                keyed.append((model.window[gid][0], len(self.witnesses), 0, gid))
        keyed.sort()
        return Assignment(frozenset(happened), tuple(node for *_k, node in keyed))


def solve(
    problem: Problem,
    *,
    budgets: Budgets = DEFAULT_BUDGETS,
    eager: bool = False,
    monolithic: bool = False,
    account: WorkAccount | None = None,
) -> Solution:
    """Decide every obligation `problem`'s inputs support (see module doc).

    Every meter also draws from `account`; an input left undecided once it
    is empty is not evaluated with HISTORY_BUDGET.
    """
    return _Run(problem, budgets, eager, monolithic, account).run()


def satisfiable(
    problem: Problem,
    *,
    work: int,
    budgets: Budgets = DEFAULT_BUDGETS,
    account: WorkAccount | None = None,
) -> tuple[str, int]:
    """Decide whether one explanation satisfies every input of `problem` within `work`.

    Returns `sat`, `unsat`, or `unknown` (the work, or `account`, ran out,
    or an input is one the engine cannot decide) and the work spent. Only
    `unsat` is final evidence. No proof is extracted and nothing is
    attributed.
    """
    return _Run(problem, budgets, False, False, account).satisfiable(work)
