# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Stratified planning and execution accounting for exploratory campaigns.

Accounting reads per-obligation verdict results: each property's
obligations by status, its declared not-evaluated reasons, an action funnel
(planned, started, recorded) by rule family and run mode, and an obligation
funnel (applicable, observed, evaluated, not-evaluated, violated) by rule
family, channel, and run mode. The two funnels count different units and
are reported separately. It also sums the checker's deterministic per-phase
work counters and component high-water marks, and states campaign health as
named checks. No check reads wall-clock time.
"""

from __future__ import annotations

import contextlib
import json
import os
from collections.abc import Callable, Iterable, Mapping
from dataclasses import asdict, dataclass, field, replace
from typing import Any, Protocol

from .checker import ACCEPT, INVALID_HISTORY, PropertyReport, registry
from .containment import durable_history
from .effects import is_canonical_key
from .facts import ActionFacts, HistoryFacts
from .generate import (
    admitted_strata,
    compose,
    held_inputs,
    lease_kinds,
    shape_signature,
    Stratum,
)
from .history import History, INFO, OK
from .jsonio import digest
from .property import (
    is_core,
    ObligationStatus,
    path_subject,
    PropertyDef,
    PropertyResult,
    SOLVER_BUDGET,
    SOLVER_EXPLANATION_BUDGET,
    SOLVER_HISTORY_BUDGET,
    SOLVER_UNKNOWN_INITIAL,
    SOLVER_WITNESS_REJECTED,
    SubjectKind,
)
from .runspec import Budgets, DEFAULT_BUDGETS, FINISHED, from_json, RunSpec
from .target import HeldInput, Target

_COMPLETED = "completed"
_HARNESS_FAILURE = "harness-failure"
_ENVIRONMENT_DEGRADED = "environment-degraded"
_CONTAINMENT_TIMEOUT = "containment-timeout"
_HARNESS_DEFECT_STREAK = 5
HARNESS_FAILURE_CEILING = 6
_RUN_SPEC = "run-spec.json"
_HISTORY = "history.jsonl"
# One run's spec, indexed durable history, and facts (None when they do not build).
_Loaded = tuple[RunSpec, History, HistoryFacts | None]
_STDERR = "stderr.log"
_VERDICT = "verdict.json"
WITNESS_FLOOR = 6
KEEP_ALL = "all"
KEEP_REJECTS = "rejects"
KEEP_CHOICES = (KEEP_ALL, KEEP_REJECTS)
REJECTS_DIR = "rejects"


class _Execution(Protocol):
    @property
    def status(self) -> str: ...

    @property
    def reason(self) -> str: ...

    @property
    def returncode(self) -> int | None: ...

    @property
    def setup(self) -> Mapping[str, Any] | None: ...


class _Verdict(Protocol):
    @property
    def kind(self) -> str: ...

    @property
    def reason(self) -> str: ...

    @property
    def not_evaluated_cause(self) -> str | None: ...

    @property
    def report(self) -> PropertyReport | None: ...


class _ExplorationResult(Protocol):
    @property
    def execution(self) -> _Execution: ...

    @property
    def verdict(self) -> _Verdict: ...

    @property
    def dir(self) -> str: ...


@dataclass(frozen=True)
class PlanEntry:
    """Bind one contiguous campaign seed to an optional forced stratum."""

    seed: int
    stratum: Stratum | None


@dataclass(frozen=True)
class CampaignAttempt:
    """Pair one actual generated stratum with its execution result."""

    seed: int
    stratum: Stratum | None
    result: _ExplorationResult


@dataclass(frozen=True)
class RejectProvenance:
    """Identify one checker rejection and its replay artifacts."""

    seed: int
    reason: str
    artifacts: str


@dataclass
class PropertyTotals:
    """Everything one property did across a campaign's runs.

    `obligations` counts its obligations by status; `not_evaluated` counts
    the declared reason of each not-evaluated obligation, which blocks a
    clean result only for its own scope. `no_obligation_runs` counts runs
    in which the property declared nothing; coverage is observability and
    never decides a finding.
    """

    runs: int = 0
    no_obligation_runs: int = 0
    obligations: dict[str, int] = field(
        default_factory=lambda: {str(status): 0 for status in ObligationStatus}
    )
    not_evaluated: dict[str, int] = field(default_factory=dict)
    errors: dict[str, int] = field(default_factory=dict)
    findings: dict[str, int] = field(default_factory=dict)
    coverage: dict[str, int] = field(default_factory=dict)


def _bump(table: dict[str, int], key: str, count: int = 1) -> None:
    table[key] = table.get(key, 0) + count


def _add_results(totals: dict[str, PropertyTotals], report: PropertyReport) -> None:
    """Fold one run's property results into the campaign totals."""
    for result in report.results:
        entry = totals.setdefault(result.property_id, PropertyTotals())
        entry.runs += 1
        entry.no_obligation_runs += not result.obligations
        for obligation in result.obligations:
            _bump(entry.obligations, str(obligation.status))
            if obligation.status == ObligationStatus.NOT_EVALUATED:
                assert obligation.not_evaluated_reason is not None
                _bump(entry.not_evaluated, obligation.not_evaluated_reason)
        if result.error is not None:
            _bump(entry.errors, result.error.type)
        for finding in result.findings:
            _bump(entry.findings, finding.rule_id)
        for obligation_id, count in result.coverage.items():
            _bump(entry.coverage, obligation_id, count)


ACTION_FUNNEL = ("planned", "started", "recorded")
OBLIGATION_FUNNEL = (
    "applicable",
    "observed",
    "evaluated",
    "not_evaluated",
    "violated",
)
NO_CHANNEL = "-"
PRIMARY_MODE = "primary"
DIFFERENTIAL_MODE = "differential"
_CHANNEL_ROLE = "channel"


def run_mode(spec: RunSpec, target: Target) -> str:
    """Return `differential` when any action owes a reference side, else `primary`."""
    owes = target.expects_reference
    if owes is not None and any(owes(spec, action) for action in spec.actions):
        return DIFFERENTIAL_MODE
    return PRIMARY_MODE


def action_subjects(
    target: Target, spec: RunSpec, action_id: str
) -> frozenset[str] | None:
    """Return the PATH subject tokens whose obligations one action's result can feed.

    They are the action's declared `input_subject` and every exact path its
    footprint reads or writes. None means unbounded: the spec does not
    declare the action, the target has no definition for its kind, its
    footprint names a subtree (ROOT included), or its declared input subject
    is not a canonical path. Non-PATH subjects (descriptors, the checkout,
    the daemon) are never named: no footprint bounds them, so callers treat
    every such subject as fed by every action.
    """
    action = next((a for a in spec.actions if a.id == action_id), None)
    definition = None if action is None else target.actions.get(action.kind)
    if action is None or definition is None:
        return None
    footprint = definition.footprint(spec, action)
    if footprint.subtrees_read or footprint.subtrees_written:
        return None
    paths = set(footprint.reads | footprint.writes)
    if definition.input_subject is not None:
        declared = definition.input_subject(spec, action)
        if not is_canonical_key(declared):
            return None
        paths.add(declared)
    return frozenset(path_subject(path).token for path in paths)


def _family_kinds(definition: PropertyDef, target: Target) -> frozenset[str]:
    """Return the action kinds whose results a family judges.

    A target property names them in `consumes`; a core property judges
    every modeled input.
    """
    if definition.consumes:
        return frozenset(kind for kind, _field in definition.consumes)
    if is_core(definition.id):
        return frozenset(
            kind
            for kind, action in target.actions.items()
            if action.modeled and action.axis == "input"
        )
    return frozenset()


def _channel(witnesses: Iterable[Any]) -> str:
    channels = sorted({w.value for w in witnesses if w.role == _CHANNEL_ROLE})
    return "+".join(channels) if channels else NO_CHANNEL


def _recorded(recorded: ActionFacts) -> bool:
    """Return whether an action's terminal result, and any reference side, decoded."""
    reference = recorded.reference
    return (
        recorded.completed is not None
        and not recorded.problems
        and (reference is None or not reference.problems)
    )


@dataclass(frozen=True)
class _Unrecorded:
    """The subjects an invoked action with no decoded result could have fed.

    `paths` are PATH subject tokens (`action_subjects`); `unbounded` means
    some such action's subjects have no bound; `any` means there is at
    least one such action, which every non-PATH subject depends on.
    """

    paths: frozenset[str] = frozenset()
    unbounded: bool = False
    any: bool = False

    def observed(self, token: str) -> bool:
        """Return whether every input an obligation on `token` reads was recorded."""
        if self.unbounded:
            return False
        if token.startswith(f"{SubjectKind.PATH}:"):
            return token not in self.paths
        return not self.any


def _unrecorded(
    spec: RunSpec, history: History, facts: HistoryFacts | None, target: Target
) -> _Unrecorded:
    """Return what the invoked actions without a decoded result could have fed."""
    if facts is None:
        return _Unrecorded(unbounded=True, any=True)
    paths: set[str] = set()
    found = False
    for action in spec.actions:
        if history.invoke(action.id) is None:
            continue
        recorded = facts.actions.get(action.id)
        if recorded is not None and _recorded(recorded):
            continue
        found = True
        fed = action_subjects(target, spec, action.id)
        if fed is None:
            return _Unrecorded(unbounded=True, any=True)
        paths |= fed
    return _Unrecorded(frozenset(paths), False, found)


def _count_row(
    table: dict[tuple[str, ...], dict[str, int]],
    key: tuple[str, ...],
    columns: tuple[str, ...],
) -> dict[str, int]:
    return table.setdefault(key, dict.fromkeys(columns, 0))


def _add_obligations(
    table: dict[tuple[str, ...], dict[str, int]],
    result: PropertyResult,
    mode: str,
    unrecorded: _Unrecorded,
) -> None:
    for obligation in result.obligations:
        row = _count_row(
            table,
            (result.property_id, _channel(obligation.witnesses), mode),
            OBLIGATION_FUNNEL,
        )
        row["applicable"] += 1
        row["observed"] += unrecorded.observed(obligation.subject.token)
        row["evaluated"] += obligation.status != ObligationStatus.NOT_EVALUATED
        row["not_evaluated"] += obligation.status == ObligationStatus.NOT_EVALUATED
        row["violated"] += obligation.status == ObligationStatus.VIOLATED


def _add_actions(
    table: dict[tuple[str, ...], dict[str, int]],
    loaded: tuple[RunSpec, History, HistoryFacts | None],
    mode: str,
    target: Target,
) -> None:
    spec, history, facts = loaded
    for definition in registry(target):
        kinds = _family_kinds(definition, target)
        planned = [a for a in spec.actions if a.kind in kinds]
        if not planned:
            continue
        counts = _count_row(table, (definition.id, mode), ACTION_FUNNEL)
        counts["planned"] += len(planned)
        counts["started"] += sum(history.invoke(a.id) is not None for a in planned)
        counts["recorded"] += sum(
            facts is not None
            and a.id in facts.actions
            and _recorded(facts.actions[a.id])
            for a in planned
        )


def funnel_accounting(
    rows: Iterable["CampaignAttempt"],
    target: Target,
    loaded: Mapping[str, _Loaded | None] | None = None,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Tabulate the action funnel and the obligation funnel, as separate rows.

    Action rows, by family and mode: `planned` counts spec actions of a kind
    the family judges, `started` those the history invoked, and `recorded`
    those whose terminal result (and any reference side) decoded.

    Obligation rows, by family, channel, and mode: `applicable` counts
    declared obligations; `observed` those whose inputs were all recorded,
    that is, no invoked action without a decoded result could have fed the
    obligation's subject (`action_subjects`; a non-PATH subject is fed by
    every action, and a run whose facts did not build observes nothing);
    `evaluated` those satisfied or violated, `not_evaluated` the rest, and
    `violated` the violations.

    A family is the property that owns the rule; the channel is the
    obligation's `channel` witness, or `-`; the mode is `run_mode`.
    `loaded` is `load_runs(rows, target)` when the caller already has it.
    """
    rows = list(rows)
    runs = load_runs(rows, target) if loaded is None else loaded
    actions: dict[tuple[str, ...], dict[str, int]] = {}
    obligations: dict[tuple[str, ...], dict[str, int]] = {}
    for row in rows:
        run = runs[row.result.dir]
        mode = PRIMARY_MODE if run is None else run_mode(run[0], target)
        unrecorded = _Unrecorded(unbounded=True, any=True)
        if run is not None:
            _add_actions(actions, run, mode, target)
            unrecorded = _unrecorded(*run, target)
        report = row.result.verdict.report
        for result in report.results if report is not None else ():
            _add_obligations(obligations, result, mode, unrecorded)
    return (
        [
            {"family": family, "mode": mode, **counts}
            for (family, mode), counts in sorted(actions.items())
        ],
        [
            {"family": family, "channel": channel, "mode": mode, **counts}
            for (family, channel, mode), counts in sorted(obligations.items())
        ],
    )


_TOTAL = "total"
_MAX = "max"
COMPONENT_PHASE = "component"
_COMPONENTS = "components"


def work_accounting(rows: Iterable["CampaignAttempt"]) -> dict[str, Any]:
    """Sum the deterministic work counters of every judged run.

    `phases[phase][counter]` holds the total over runs and the largest
    single-run value. `components[counter]` is the largest value any one
    solver component reached in any run, and `components["components"]`
    the most components one run had. Wall-clock time is never counted.
    """
    phases: dict[str, dict[str, dict[str, int]]] = {}
    components: dict[str, int] = {}
    runs = 0
    for row in rows:
        report = row.result.verdict.report
        if report is None or not report.results:
            continue
        runs += 1
        for phase, counters in report.work.phases.items():
            table = phases.setdefault(phase, {})
            for counter, value in counters.items():
                entry = table.setdefault(counter, {_TOTAL: 0, _MAX: 0})
                entry[_TOTAL] += value
                entry[_MAX] = max(entry[_MAX], value)
        components[_COMPONENTS] = max(
            components.get(_COMPONENTS, 0), len(report.work.components)
        )
        for counters in report.work.components.values():
            for counter, value in counters.items():
                components[counter] = max(components.get(counter, 0), value)
    return {
        "runs": runs,
        "phases": {
            phase: dict(sorted(table.items()))
            for phase, table in sorted(phases.items())
        },
        "components": dict(sorted(components.items())),
    }


def cause_accounting(rows: Iterable["CampaignAttempt"]) -> dict[str, int]:
    """Count not-evaluated verdicts by their cause."""
    causes: dict[str, int] = {}
    for row in rows:
        cause = row.result.verdict.not_evaluated_cause
        if row.result.verdict.kind == "not-evaluated" and cause is not None:
            _bump(causes, cause)
    return dict(sorted(causes.items()))


@dataclass(frozen=True)
class CampaignReport:
    """Campaign counts, coverage, and checker rejection provenance."""

    attempts: int
    completed: int
    harness_failures: int
    environment_degraded: int
    verdicts: dict[str, int]
    completed_strata: dict[str, int]
    missing_strata: tuple[Stratum, ...]
    rejects: tuple[RejectProvenance, ...]
    properties: dict[str, PropertyTotals] = field(default_factory=dict)
    stopped: dict[str, Any] | None = None
    excluded_sites: tuple[dict[str, str], ...] = ()
    leaked_run_dirs: tuple[dict[str, Any], ...] = ()
    sites: dict[str, dict[str, int]] = field(default_factory=dict)
    input_families: dict[str, dict[str, int]] = field(default_factory=dict)
    under_witnessed_families: tuple[str, ...] = ()
    actions: tuple[dict[str, Any], ...] = ()
    obligations: tuple[dict[str, Any], ...] = ()
    work: dict[str, Any] = field(default_factory=lambda: work_accounting(()))
    not_evaluated_causes: dict[str, int] = field(default_factory=dict)

    @property
    def harness_failure_rate(self) -> float:
        """Return harness failures divided by every attempted run."""
        return self.harness_failures / self.attempts if self.attempts else 0.0


def plan(
    target: Target, seed_start: int, seeds: int, daemon_label: str
) -> tuple[PlanEntry, ...]:
    """Assign every seed a stratum by cycling admitted sites, then strata.

    Round-robin over sites keeps every admitted site near an equal share;
    round-robin within each site keeps every admitted stratum covered.
    """
    strata = admitted_strata(target, daemon_label)
    if not strata:
        return tuple(
            PlanEntry(seed, None) for seed in range(seed_start, seed_start + seeds)
        )
    by_site: dict[str, list[Stratum]] = {}
    for stratum in strata:
        by_site.setdefault(stratum[0], []).append(stratum)
    sites = sorted(by_site)
    entries: list[PlanEntry] = []
    for index, seed in enumerate(range(seed_start, seed_start + seeds)):
        rounds = by_site[sites[index % len(sites)]]
        entries.append(PlanEntry(seed, rounds[(index // len(sites)) % len(rounds)]))
    return tuple(entries)


def _stratum_key(stratum: Stratum) -> str:
    """Return the stable serialized key used by campaign reports."""
    return "/".join(stratum)


def _load_run(artifact_dir: str, target: Target) -> _Loaded | None:
    """Load a run's spec, indexed durable history, and its facts when both artifacts exist.

    The history is the durable prefix containment bound
    (`containment.durable_history`); a torn or invalid tail never aborts a
    campaign. Facts are None when that prefix breaks the spec's grammar. An
    unreadable or invalid spec loads as no run, the same as a missing one.
    """
    spec_path = os.path.join(artifact_dir, _RUN_SPEC)
    history_path = os.path.join(artifact_dir, _HISTORY)
    if not artifact_dir or not all(map(os.path.isfile, (spec_path, history_path))):
        return None
    try:
        with open(spec_path) as handle:
            spec = from_json(handle.read(), target)
    except (OSError, ValueError):
        return None
    events = durable_history(history_path)
    try:
        facts: HistoryFacts | None = HistoryFacts.build(spec, events, target)
    except ValueError:
        facts = None
    return spec, History(spec, events, target.actions), facts


def load_runs(
    rows: Iterable["CampaignAttempt"], target: Target
) -> dict[str, _Loaded | None]:
    """Load each attempt's run artifacts once, by artifact dir, for every accounting pass."""
    return {row.result.dir: _load_run(row.result.dir, target) for row in rows}


def _install_witnessed(
    spec: RunSpec, history: History, facts: HistoryFacts | None, target: Target
) -> tuple[bool, bool]:
    """Return whether a run's fault install completed `ok` and whether the
    target saw one of its installs fire (never, without valid facts)."""
    installs = history.fault_installs()
    return bool(installs), facts is not None and any(
        target.install_witnessed(spec, facts, a) for a in installs
    )


def _site_table(
    rows: list[CampaignAttempt],
    strata: tuple[Stratum, ...],
    target: Target,
    runs: Mapping[str, _Loaded | None],
) -> dict[str, dict[str, int]]:
    """Tabulate admitted strata, installs, witnessed installs, and completions per site."""
    table: dict[str, dict[str, int]] = {}
    for stratum in strata:
        site = table.setdefault(
            stratum[0], {"admitted": 0, "installs": 0, "witnessed": 0, "completed": 0}
        )
        site["admitted"] += 1
    if not table:
        return table
    for row in rows:
        if row.stratum is None or row.stratum[0] not in table:
            continue
        site = table[row.stratum[0]]
        loaded = runs[row.result.dir]
        installed, seen = (
            _install_witnessed(*loaded, target) if loaded else (False, False)
        )
        site["installs"] += installed
        site["witnessed"] += seen
        site["completed"] += row.result.execution.status == _COMPLETED
    return table


def _leaked_run_dir(seed: int, result: _ExplorationResult) -> list[dict[str, Any]]:
    """Return one `{seed, path}` row when the run's recorded run dir still exists."""
    path = (result.execution.setup or {}).get("run_dir")
    return [{"seed": seed, "path": path}] if path and os.path.isdir(path) else []


def _lease_landed(history: History, lease_id: str) -> bool:
    """Return whether a lease ran inside a nemesis window other than its own.

    The lease's own window is the one `nemesis_windows` opens for it: it
    never closes when the lease completed `info`.
    """
    interval = history.interval(lease_id)
    completion = history.completion(lease_id)
    if interval is None or completion is None:
        return False
    own = (interval[0], float("inf") if completion.type == INFO else interval[1])
    windows = history.nemesis_windows()
    if own in windows:
        windows.remove(own)
    return any(start <= interval[1] and interval[0] <= end for start, end in windows)


def _stream_held(
    spec: RunSpec, history: History, held: HeldInput, stream_id: str
) -> bool:
    """Return whether the stream paused `ok` before its partner and drained `ok` after it."""
    edge = (stream_id, FINISHED)
    partners = [
        a.id for a in spec.actions if a.kind == held.partner and edge in a.requires
    ]
    resumes = [
        a.id
        for a in spec.actions
        if a.kind == held.resume and a.payload.get(held.resume_field) == stream_id
    ]
    if len(partners) != 1 or len(resumes) != 1:
        return False
    partner_done = history.completion(partners[0])
    resume_done = history.completion(resumes[0])
    return (
        history.outcome(stream_id) == OK
        and history.outcome(resumes[0]) == OK
        and history.completed_before(stream_id, partners[0])
        and partner_done is not None
        and resume_done is not None
        and resume_done.time > partner_done.time
    )


def _family_witnesses(
    spec: RunSpec, history: History, target: Target
) -> dict[str, bool]:
    """Return, per input family present in the spec, whether every instance was witnessed."""
    witnesses: dict[str, bool] = {}
    leases = lease_kinds(target)
    held_by_stream = {held.stream: held for held in held_inputs(target)}
    for action in spec.actions:
        if action.kind in leases:
            witnessed = _lease_landed(history, action.id)
        elif action.kind in held_by_stream:
            witnessed = _stream_held(
                spec, history, held_by_stream[action.kind], action.id
            )
        else:
            continue
        witnesses[action.kind] = witnesses.get(action.kind, True) and witnessed
    return witnesses


def _family_table(
    rows: list[CampaignAttempt], target: Target, runs: Mapping[str, _Loaded | None]
) -> dict[str, dict[str, int]]:
    """Tabulate specs, completed, witnessed, and exit-unobserved runs per input family.

    An instance's exit is unobserved when it completed `info`.
    """
    table = {
        family: {"specs": 0, "completed": 0, "witnessed": 0, "exit_unobserved": 0}
        for family in (*lease_kinds(target), *(h.stream for h in held_inputs(target)))
    }
    for row in rows:
        loaded = runs[row.result.dir]
        if loaded is None:
            continue
        spec, history = loaded[0], loaded[1]
        unobserved = {
            action.kind
            for action in spec.actions
            if (completion := history.completion(action.id)) is not None
            and completion.type == INFO
        }
        for family, witnessed in _family_witnesses(spec, history, target).items():
            counts = table[family]
            counts["specs"] += 1
            counts["completed"] += row.result.execution.status == _COMPLETED
            counts["witnessed"] += witnessed
            counts["exit_unobserved"] += family in unobserved
    return table


def account(
    attempts: Iterable[CampaignAttempt],
    strata: Iterable[Stratum],
    target: Target,
    loaded: Mapping[str, _Loaded | None] | None = None,
) -> CampaignReport:
    """Count outcomes while requiring completed runs for stratum coverage.

    `loaded` is `load_runs(attempts, target)` when the caller already has it.
    """
    rows = list(attempts)
    runs = load_runs(rows, target) if loaded is None else loaded
    observable = tuple(strata)
    statuses = [row.result.execution.status for row in rows]
    verdicts: dict[str, int] = {}
    completed_strata: dict[str, int] = {}
    rejects: list[RejectProvenance] = []
    properties: dict[str, PropertyTotals] = {}
    for row in rows:
        result = row.result
        if result.verdict.report is not None:
            _add_results(properties, result.verdict.report)
        verdicts[result.verdict.kind] = verdicts.get(result.verdict.kind, 0) + 1
        if result.verdict.kind == "reject":
            rejects.append(
                RejectProvenance(row.seed, result.verdict.reason, result.dir)
            )
        if row.stratum is not None and result.execution.status == _COMPLETED:
            key = _stratum_key(row.stratum)
            completed_strata[key] = completed_strata.get(key, 0) + 1
    missing = tuple(s for s in observable if _stratum_key(s) not in completed_strata)
    action_rows, obligation_rows = funnel_accounting(rows, target, runs)
    return CampaignReport(
        attempts=len(rows),
        completed=statuses.count(_COMPLETED),
        harness_failures=statuses.count(_HARNESS_FAILURE),
        environment_degraded=statuses.count(_ENVIRONMENT_DEGRADED),
        verdicts=verdicts,
        completed_strata=completed_strata,
        missing_strata=missing,
        rejects=tuple(rejects),
        properties=dict(sorted(properties.items())),
        sites=_site_table(rows, observable, target, runs),
        actions=tuple(action_rows),
        obligations=tuple(obligation_rows),
        work=work_accounting(rows),
        not_evaluated_causes=cause_accounting(rows),
    )


def retain(seed_dir: str, result: _ExplorationResult, keep: str) -> str:
    """Apply the retention policy to one finished seed and return its directory.

    `rejects` drops an accepted seed's stderr and moves every other seed
    under `rejects/`, so the nightly uploads only what a human will read.
    """
    if keep == KEEP_ALL:
        return seed_dir
    accepted = result.execution.status == _COMPLETED and result.verdict.kind == ACCEPT
    if accepted:
        with contextlib.suppress(FileNotFoundError):
            os.remove(os.path.join(seed_dir, _STDERR))
        return seed_dir
    rejects = os.path.join(os.path.dirname(seed_dir), REJECTS_DIR)
    os.makedirs(rejects, exist_ok=True)
    final = os.path.join(rejects, os.path.basename(seed_dir))
    os.rename(seed_dir, final)
    return final


REPORT_SCHEMA_VERSION = 1
REPORT_KEYS = (
    "strata",
    "seeds",
    "completed",
    "verdicts",
    "harness_failures",
    "environment_degraded",
    "rate",
    "properties",
    "rejects",
    "excluded_sites",
    "leaked_run_dirs",
    "sites",
    "input_families",
    "under_witnessed_families",
    "actions",
    "obligations",
    "work",
    "not_evaluated_causes",
)
REPORT_OPTIONAL_KEYS = frozenset(
    {
        "stopped",
        "campaign_id",
        "daemon_identity",
        "daemon_sha256",
        "daemon_source_commit",
        "fatal_eden_bug",
        "daemon_log",
        "binaries",
        "diagnostics",
    }
)
ROW_BYTES = 8 * 1024
ROW_OVERSIZE = "telemetry.row-oversize"
SYMPTOM_ROWS_PER_RUN = 16384
_REASON_FIELDS = frozenset(
    {"row_type", "property_id", "reason_kind", "reason_id", "count"}
)
# The exact fields of each telemetry row type. Every field is a deterministic
# count, flag, or identifier; `validate_row` refuses any other field, so a
# wall-clock, elapsed-time, or resource-usage value can only reach telemetry
# by being added here.
ROW_FIELDS: Mapping[str, frozenset[str]] = {
    "campaign": frozenset(
        {
            "row_type",
            "campaign_id",
            "daemon_sha256",
            "daemon_source_commit",
            "fatal_eden_bug",
            "seeds",
            "completed",
            "harness_failures",
            "environment_degraded",
            "accept",
            "reject",
            "harness_suspect",
            "not_evaluated",
            "stopped_reason",
            "per_site_witnessed_min",
            "families_witnessed_min",
            "task_keys",
            "unclassified",
            "routed_harness",
            "routed_reference",
            "execution_findings",
            "health_passed",
            "health_failed",
            "controls_supplied",
            "controls_passed",
        }
    ),
    "property": frozenset(
        {
            "row_type",
            "property_id",
            "runs",
            "no_obligation_runs",
            "satisfied",
            "violated",
            "not_evaluated",
            "findings",
            "errors",
            "coverage_observations",
        }
    ),
    "reason": _REASON_FIELDS,
    "action": frozenset({"row_type", "family", "mode", *ACTION_FUNNEL}),
    "obligation": frozenset(
        {"row_type", "family", "channel", "mode", *OBLIGATION_FUNNEL}
    ),
    "work": frozenset({"row_type", "phase", "counter", "runs", "total", "max"}),
    "symptom": frozenset(
        {"row_type", "task_key", "rule_id", "finding_kind", "roles_digest"}
    ),
    "finding": frozenset({"row_type", "task_key", "rules", "runs", "first_seed"}),
}
_SCALARS = (str, int, float, bool, type(None))


# Not-evaluated reasons that mean the engine ran out of budget or facts
# rather than that the history left an obligation undecidable.
ENGINE_SHORTFALLS = (
    SOLVER_BUDGET,
    SOLVER_EXPLANATION_BUDGET,
    SOLVER_HISTORY_BUDGET,
    SOLVER_UNKNOWN_INITIAL,
    SOLVER_WITNESS_REJECTED,
)


@dataclass(frozen=True)
class HealthCheck:
    """One named campaign-health check: whether it passed and, if not, why."""

    name: str
    ok: bool
    detail: str = ""


def _under_floor(table: Mapping[str, Mapping[str, int]]) -> str:
    return ", ".join(
        f"{name}={counts['witnessed']}"
        for name, counts in sorted(table.items())
        if counts["witnessed"] < WITNESS_FLOOR
    )


def campaign_health(report: Mapping[str, Any]) -> tuple[HealthCheck, ...]:
    """Return a campaign's health checks, computed from its report alone.

    Health says whether the campaign exercised the system well enough to
    promote it; it never admits or blocks an already proven finding. Every
    check reads deterministic counts: runs that neither completed nor were
    environment degraded (harness failures and containment timeouts), an early stop,
    stratum and site coverage, property errors and invalid histories,
    leaked run directories, whether any obligation was judged, and engine
    shortfalls (budget, missing-fact, or rejected-witness not-evaluated
    obligations). The
    input-family table stays in the report and gates nothing. None reads
    elapsed time.
    """
    failures = report["seeds"] - report["completed"] - report["environment_degraded"]
    missing = report["strata"]["missing"]
    leaked = len(report["leaked_run_dirs"])
    stopped = report.get("stopped")
    totals = report["properties"].values()
    judged = sum(
        t["obligations"].get(str(s), 0)
        for t in totals
        for s in (ObligationStatus.SATISFIED, ObligationStatus.VIOLATED)
    )
    shortfalls = {
        reason: sum(t["not_evaluated"].get(reason, 0) for t in totals)
        for reason in ENGINE_SHORTFALLS
    }
    short = ", ".join(f"{r}={n}" for r, n in shortfalls.items() if n)
    sites = _under_floor(report["sites"])
    errors = report["not_evaluated_causes"].get(INVALID_HISTORY, 0) + sum(
        sum(p["errors"].values()) for p in report["properties"].values()
    )
    return (
        HealthCheck(
            "harness-failures",
            failures <= HARNESS_FAILURE_CEILING,
            f"{failures} > {HARNESS_FAILURE_CEILING}"
            if failures > HARNESS_FAILURE_CEILING
            else "",
        ),
        HealthCheck(
            "not-stopped", stopped is None, "" if stopped is None else stopped["reason"]
        ),
        HealthCheck("strata-covered", not missing, f"{len(missing)} missing"),
        HealthCheck("sites-witnessed", not sites, sites),
        HealthCheck(
            "no-property-errors", not errors, f"{errors} errors" if errors else ""
        ),
        HealthCheck(
            "no-leaked-run-dirs", not leaked, f"{leaked} leaked" if leaked else ""
        ),
        HealthCheck(
            "obligations-judged",
            judged > 0,
            "" if judged else "no obligation was satisfied or violated",
        ),
        HealthCheck("no-engine-shortfall", not short, short),
    )


def finding_rows(verdict: _Verdict) -> list[dict[str, Any]]:
    """Return every finding of a verdict as typed scalars, in property and encounter order."""
    report = verdict.report
    if report is None:
        return []
    return [
        {
            "property_id": result.property_id,
            "rule_id": finding.rule_id,
            "attribution": str(finding.attribution),
            "subject": finding.subject.token,
        }
        for result in report.results
        for finding in result.findings
    ]


def summary_row(
    seed: int,
    stratum: Stratum | None,
    signature: Any,
    result: _ExplorationResult,
) -> dict[str, Any]:
    """Return the one `summary.jsonl` row for a finished seed.

    It carries every typed finding and the verdict's encoded size; the
    compatibility kind and reason stay for human reading only.
    """
    verdict_path = os.path.join(result.dir, _VERDICT)
    return {
        "seed": seed,
        "stratum": stratum,
        "shape_signature": signature,
        "status": result.execution.status,
        "reason": result.execution.reason,
        "verdict": result.verdict.kind,
        "verdict_reason": result.verdict.reason,
        "not_evaluated_cause": result.verdict.not_evaluated_cause,
        "findings": finding_rows(result.verdict),
        "verdict_bytes": os.path.getsize(verdict_path)
        if os.path.isfile(verdict_path)
        else None,
        "artifacts": result.dir,
    }


def write_summary_row(handle: Any, row: Mapping[str, Any]) -> None:
    """Append one summary row as canonical JSONL."""
    handle.write(json.dumps(row, sort_keys=True) + "\n")


def validate_row(row: Mapping[str, Any]) -> dict[str, Any]:
    """Require a known row type, exactly its `ROW_FIELDS`, scalar values, and at most `ROW_BYTES`.

    An oversized row becomes a typed `reason` row naming its type and size;
    its full content stays in the findings and report artifacts.
    """
    row_type = row.get("row_type")
    if row_type not in ROW_FIELDS:
        raise ValueError(f"unknown row type {row_type!r}")
    expected = ROW_FIELDS[row_type]
    if set(row) != expected:
        extra, missing = sorted(set(row) - expected), sorted(expected - set(row))
        raise ValueError(f"{row_type} row fields: extra {extra}, missing {missing}")
    for key, value in row.items():
        if not isinstance(value, _SCALARS):
            raise ValueError(f"row field {key!r} is not scalar: {value!r}")
    encoded = json.dumps(row, sort_keys=True).encode("utf-8")
    if len(encoded) > ROW_BYTES:
        return {
            "row_type": "reason",
            "property_id": "telemetry",
            "reason_kind": str(row["row_type"]),
            "reason_id": ROW_OVERSIZE,
            "count": len(encoded),
        }
    return dict(row)


def _campaign_row(
    report: Mapping[str, Any], findings: Mapping[str, Any] | None
) -> dict[str, Any]:
    stopped = report.get("stopped")
    verdicts = report["verdicts"]
    controls = None if findings is None else findings["controls"]

    def witnessed_min(table: Mapping[str, Mapping[str, int]]) -> int | None:
        return min((counts["witnessed"] for counts in table.values()), default=None)

    health = campaign_health(report)

    return {
        "row_type": "campaign",
        "campaign_id": report.get("campaign_id"),
        "daemon_sha256": report.get("daemon_sha256"),
        "daemon_source_commit": report.get("daemon_source_commit"),
        "fatal_eden_bug": report.get("fatal_eden_bug"),
        "seeds": report["seeds"],
        "completed": report["completed"],
        "harness_failures": report["harness_failures"],
        "environment_degraded": report["environment_degraded"],
        "accept": verdicts.get("accept", 0),
        "reject": verdicts.get("reject", 0),
        "harness_suspect": verdicts.get("harness-suspect", 0),
        "not_evaluated": verdicts.get("not-evaluated", 0),
        "stopped_reason": None if stopped is None else stopped["reason"],
        "per_site_witnessed_min": witnessed_min(report["sites"]),
        "families_witnessed_min": witnessed_min(report["input_families"]),
        "task_keys": 0 if findings is None else len(findings["findings"]),
        "unclassified": 0 if findings is None else len(findings["unclassified"]),
        "routed_harness": 0
        if findings is None
        else sum(r["route"] == "harness" for r in findings["routed"]),
        "routed_reference": 0
        if findings is None
        else sum(r["route"] == "reference" for r in findings["routed"]),
        "execution_findings": 0 if findings is None else len(findings["execution"]),
        "health_passed": all(check.ok for check in health),
        "health_failed": ",".join(check.name for check in health if not check.ok),
        "controls_supplied": controls is not None and bool(controls),
        "controls_passed": bool(controls)
        and all(
            outcome == "pass"
            for per_rule in (controls or {}).values()
            for outcome in per_rule.values()
        ),
    }


def _property_rows(report: Mapping[str, Any]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for property_id, totals in sorted(report["properties"].items()):
        obligations = totals["obligations"]
        rows.append(
            {
                "row_type": "property",
                "property_id": property_id,
                "runs": totals["runs"],
                "no_obligation_runs": totals["no_obligation_runs"],
                "satisfied": obligations.get("satisfied", 0),
                "violated": obligations.get("violated", 0),
                "not_evaluated": obligations.get("not-evaluated", 0),
                "findings": sum(totals["findings"].values()),
                "errors": sum(totals["errors"].values()),
                "coverage_observations": sum(totals["coverage"].values()),
            }
        )
        for kind in ("not_evaluated", "errors"):
            for reason, count in sorted(totals[kind].items()):
                rows.append(
                    {
                        "row_type": "reason",
                        "property_id": property_id,
                        "reason_kind": kind,
                        "reason_id": reason,
                        "count": count,
                    }
                )
    return rows


def _funnel_rows(report: Mapping[str, Any]) -> list[dict[str, Any]]:
    return [{"row_type": "action", **row} for row in report["actions"]] + [
        {"row_type": "obligation", **row} for row in report["obligations"]
    ]


def _work_rows(report: Mapping[str, Any]) -> list[dict[str, Any]]:
    """One row per phase counter (total and largest run), then component high-water marks."""
    work = report["work"]
    rows: list[dict[str, Any]] = [
        {
            "row_type": "work",
            "phase": phase,
            "counter": counter,
            "runs": work["runs"],
            "total": values[_TOTAL],
            "max": values[_MAX],
        }
        for phase, counters in work["phases"].items()
        for counter, values in counters.items()
    ]
    rows += [
        {
            "row_type": "work",
            "phase": COMPONENT_PHASE,
            "counter": counter,
            "runs": work["runs"],
            "total": None,
            "max": value,
        }
        for counter, value in work["components"].items()
    ]
    return rows


def _cause_rows(report: Mapping[str, Any]) -> list[dict[str, Any]]:
    return [
        {
            "row_type": "reason",
            "property_id": "verdict",
            "reason_kind": "not_evaluated_cause",
            "reason_id": cause,
            "count": count,
        }
        for cause, count in report["not_evaluated_causes"].items()
    ]


def _finding_rows(findings: Mapping[str, Any]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for entry in findings["findings"]:
        seen: set[tuple[str, str, str]] = set()
        for rule_id, kind, roles in entry["errors"]:
            identity = (rule_id, kind, digest(roles))
            if identity in seen:
                continue
            seen.add(identity)
            rows.append(
                {
                    "row_type": "symptom",
                    "task_key": entry["task_key"],
                    "rule_id": rule_id,
                    "finding_kind": kind,
                    "roles_digest": identity[2],
                }
            )
        rows.append(
            {
                "row_type": "finding",
                "task_key": entry["task_key"],
                "rules": len(entry["rules"]),
                "runs": len(entry["runs"]),
                "first_seed": entry["runs"][0]["seed"],
            }
        )
    for finding in findings["execution"]:
        rows.append(
            {
                "row_type": "reason",
                "property_id": "execution",
                "reason_kind": "execution",
                "reason_id": finding["execution_id"],
                "count": 1,
            }
        )
    return rows


def report_rows(
    report: Mapping[str, Any], findings: Mapping[str, Any] | None = None
) -> list[dict[str, Any]]:
    """Flatten a campaign report and its findings into fixed scalar rows.

    Rows are `campaign` (with its health checks), then `property` with its
    property-scoped not-evaluated and error `reason` rows, a `reason` row
    per verdict-level not-evaluated cause, one `action` funnel row per
    family and mode and one `obligation` funnel row per family, channel,
    and mode, one `work` row per deterministic phase counter and component
    high-water mark, then a `symptom` row per canonical error of every
    reported key and a `finding` row per key. Witnesses, paths, prose,
    URLs, and wall-clock times stay out.
    """
    rows = [
        _campaign_row(report, findings),
        *_property_rows(report),
        *_cause_rows(report),
        *_funnel_rows(report),
        *_work_rows(report),
    ]
    if findings is not None:
        rows += _finding_rows(findings)
    symptoms = sum(row["row_type"] == "symptom" for row in rows)
    if symptoms > SYMPTOM_ROWS_PER_RUN * max(1, report["seeds"]):
        rows = [row for row in rows if row["row_type"] != "symptom"]
        rows.append(
            {
                "row_type": "reason",
                "property_id": "telemetry",
                "reason_kind": "symptom",
                "reason_id": "telemetry.symptom-rows-over-bound",
                "count": symptoms,
            }
        )
    return [validate_row(row) for row in rows]


def report_document(
    report: CampaignReport, extra: Mapping[str, Any] | None = None
) -> dict[str, Any]:
    """Return the one campaign report document; `extra` adds provenance fields."""
    document: dict[str, Any] = {
        "schema_version": REPORT_SCHEMA_VERSION,
        "strata": {
            "completed": report.completed_strata,
            "missing": [list(stratum) for stratum in report.missing_strata],
        },
        "seeds": report.attempts,
        "completed": report.completed,
        "verdicts": report.verdicts,
        "harness_failures": report.harness_failures,
        "environment_degraded": report.environment_degraded,
        "rate": report.harness_failure_rate,
        "properties": {k: asdict(v) for k, v in report.properties.items()},
        "rejects": [asdict(rejection) for rejection in report.rejects],
        "excluded_sites": list(report.excluded_sites),
        "leaked_run_dirs": list(report.leaked_run_dirs),
        "sites": report.sites,
        "input_families": report.input_families,
        "under_witnessed_families": list(report.under_witnessed_families),
        "actions": [dict(row) for row in report.actions],
        "obligations": [dict(row) for row in report.obligations],
        "work": report.work,
        "not_evaluated_causes": report.not_evaluated_causes,
        **dict(extra or {}),
    }
    if report.stopped is not None:
        document["stopped"] = report.stopped
    return document


def write_report(
    path: str, report: CampaignReport, extra: Mapping[str, Any] | None = None
) -> None:
    """Write the campaign report document durably: a temporary file in the same
    directory is fsynced and atomically replaces `path`."""
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    temporary = os.path.join(os.path.dirname(path), f".{os.path.basename(path)}.tmp")
    with open(temporary, "w") as handle:
        json.dump(report_document(report, extra), handle, sort_keys=True, indent=2)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def _write_reports(
    artifact_dir: str,
    target: Target,
    entries: tuple[PlanEntry, ...],
    strata: tuple[Stratum, ...],
    report: CampaignReport,
    extra: Mapping[str, Any] | None,
) -> None:
    """Write ordinary per-stratum coverage and aggregate report artifacts."""
    planned = {
        _stratum_key(stratum): sum(entry.stratum == stratum for entry in entries)
        for stratum in strata
    }
    strata_document = {
        "observable": [
            {
                "stratum": list(stratum),
                "planned": planned[_stratum_key(stratum)],
                "completed": report.completed_strata.get(_stratum_key(stratum), 0),
            }
            for stratum in strata
        ],
        "excluded": [],
    }
    with open(os.path.join(artifact_dir, "strata.json"), "w") as handle:
        json.dump(strata_document, handle, sort_keys=True, indent=2)
        handle.write("\n")
    write_report(os.path.join(artifact_dir, "report.json"), report, extra)


def _harness_failure_signature(
    result: _ExplorationResult,
) -> tuple[str, str, str] | None:
    """Return the comparable identity of one harness failure."""
    if result.execution.status != _HARNESS_FAILURE:
        return None
    return (
        result.execution.reason,
        result.verdict.kind,
        result.verdict.reason,
    )


def _stops_for_harness_defect(
    result: _ExplorationResult,
    previous: tuple[str, str, str] | None,
    count: int,
) -> tuple[bool, tuple[str, str, str] | None, int]:
    """Stop campaigns on signal exits or repeated identical harness failures.

    A containment timeout's runner dies by the supervisor's own SIGKILL, so
    its signal exit never stops the campaign.
    """
    returncode = result.execution.returncode
    if (
        returncode is not None
        and returncode < 0
        and result.execution.status != _CONTAINMENT_TIMEOUT
    ):
        return True, None, 0
    signature = _harness_failure_signature(result)
    if signature is None:
        return False, None, 0
    count = count + 1 if signature == previous else 1
    return count >= _HARNESS_DEFECT_STREAK, signature, count


def _stop_document(
    result: _ExplorationResult,
    reason: str,
    signature: tuple[str, str, str] | None,
) -> dict[str, Any]:
    """Describe the early stop in the campaign receipt."""
    if signature is None:
        signature = (
            result.execution.reason,
            result.verdict.kind,
            result.verdict.reason,
        )
    return {
        "reason": reason,
        "seed": None,
        "signature": list(signature),
    }


def run_campaign(
    target: Target,
    seeds: int,
    seed_start: int,
    environment_for_seed: Callable[[int], Mapping[str, Any]],
    artifact_dir: str,
    runner: Callable[[RunSpec, str, Target], _ExplorationResult],
    daemon_label: str,
    *,
    no_fault: bool = False,
    differential: bool = False,
    budgets: Budgets = DEFAULT_BUDGETS,
    extra: Mapping[str, Any] | None = None,
) -> CampaignReport:
    """Execute a contiguous plan and write summary, strata, and report artifacts.

    With no_fault, every seed is composed without the fault stage, so no
    stratum is planned or observable. With differential, every seed is
    composed as a paired run (`compose(differential=True)`: no fault stage,
    every pair with conflicting footprints ordered by a finished edge) over the
    environments `environment_for_seed` supplies, which must opt into the
    reference side. Every composed RunSpec carries `budgets`. `extra`
    carries provenance into the report, which is written once. A non-empty
    `artifact_dir` is refused before any seed runs.
    """
    if os.path.isdir(artifact_dir) and os.listdir(artifact_dir):
        raise ValueError(f"artifact dir {artifact_dir!r} is not empty")
    os.makedirs(artifact_dir, exist_ok=True)
    if no_fault or differential:
        entries = tuple(
            PlanEntry(seed, None) for seed in range(seed_start, seed_start + seeds)
        )
        strata: tuple[Stratum, ...] = ()
    else:
        entries = plan(target, seed_start, seeds, daemon_label)
        strata = admitted_strata(target, daemon_label)
    attempts: list[CampaignAttempt] = []
    leaked: list[dict[str, Any]] = []
    harness_failure: tuple[str, str, str] | None = None
    harness_failure_count = 0
    stopped: dict[str, Any] | None = None
    with open(os.path.join(artifact_dir, "summary.jsonl"), "w") as summary:
        for entry in entries:
            spec = replace(
                compose(
                    target,
                    entry.seed,
                    environment_for_seed(entry.seed),
                    stratum=entry.stratum,
                    forced=entry.stratum is not None,
                    no_fault=no_fault and not differential,
                    differential=differential,
                ),
                budgets=budgets,
            )
            signature = shape_signature(spec, target)
            actual = tuple(signature[:3]) if signature[0] is not None else None
            seed_dir = os.path.join(artifact_dir, f"seed-{entry.seed}")
            result = runner(spec, seed_dir, target)
            attempts.append(CampaignAttempt(entry.seed, actual, result))
            leaked.extend(_leaked_run_dir(entry.seed, result))
            stop, harness_failure, harness_failure_count = _stops_for_harness_defect(
                result, harness_failure, harness_failure_count
            )
            row = summary_row(entry.seed, actual, signature, result)
            row["artifacts"] = os.path.relpath(result.dir, artifact_dir)
            write_summary_row(summary, row)
            if stop:
                stopped = _stop_document(
                    result,
                    "signal" if harness_failure is None else "streak",
                    harness_failure,
                )
                stopped["seed"] = entry.seed
                break
    runs = load_runs(attempts, target)
    report = account(attempts, strata, target, runs)
    admitted = {site for site, _, _ in admitted_strata(target, daemon_label)}
    excluded = sorted(set(target.sites) - admitted)
    families = _family_table(attempts, target, runs)
    report = replace(
        report,
        stopped=stopped,
        leaked_run_dirs=tuple(leaked),
        excluded_sites=tuple(
            {
                "site": s,
                "reason": f"site daemon '{target.sites[s].daemon}'"
                f" not admitted on '{daemon_label}'",
            }
            for s in excluded
        ),
        input_families=families,
        under_witnessed_families=tuple(
            sorted(f for f, c in families.items() if c["witnessed"] < WITNESS_FLOOR)
        ),
    )
    _write_reports(artifact_dir, target, entries, strata, report, extra)
    return report
