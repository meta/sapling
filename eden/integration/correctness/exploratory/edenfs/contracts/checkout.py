# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Checkout contracts: Thrift and `sl goto`, interruption, resume, refusal,
conflict counts, and the observations checkouts and status make.

The `source` cells record which revision a checkout started from: its
parent group, evaluated on the values just before it, marks the one
candidate the EdenFS parent held (or, while the `interrupted` cell names
an interrupted checkout to the same destination, that checkout's source: a
resume diffs against it). Every per-path group of a checkout that is not
settled requires that some mark of its own checkout is set. That orders
the parent move before every path change even when the destination is the
revision the checkout starts from, and ties an interrupted checkout's
paths to its start. When another checkout's parent move may land inside
the window, the group also requires the parent at its destination, which
serializes two checkouts to different revisions. No per-path group reads
the parent cell otherwise, so paths couple only through their own
checkout's marks. A per-path group reads its source from the mark; before
any mark exists every candidate's rows apply, so a path group never turns
into a no-op merely by running before its own parent group: FORCE to the
current revision still resets every path. Two checkouts may therefore both
apply, one after the other; no Exclusive is derived, because without
server timestamps no pair provably overlapped at the daemon. A `visit`
cell records that a checkout's per-path group has applied its path.

A checkout is settled (`_plans`) when every parent move that may precede
it is acked and ordered, so its source is one known revision, and no
other parent move, checkout, or aggregate observation overlaps its
window, and no attempt to the same revision that may have been
interrupted started before its window ends (it could resume into this
one): then nothing can read the parent between its parent group and
its paths, its groups read no mark, and no order requirement is
derived. Its paths then couple to nothing but their own path's actions.

A `conflict` cell records that an acked Thrift checkout reported a path
as conflicting: its per-path group marks it when the pre-visit state is
a conflict state, and one observation at the
checkout's completion, after every marking group, requires the marked
paths' weight to equal the recorded count (`count_inputs`). A write,
chmod, or unlink of a counted path is ordered by the engine against that
group: counted when it lands before the visit (kept by NORMAL, reset by
FORCE), not counted after it. A directory the destination removes or
turns into a non-directory is counted once more (DIRECTORY_NOT_EMPTY)
when NORMAL keeps a conflicting path below it: that path's group marks
the directory too (`_nonempty_markers`); a class representative there
weighs a range (`_CountPlan`). A count whose
relevant paths include a type change, a path created with its parent
directory, a directory the run moves, or, below a removed directory, a
path neither revision holds is not derived: how EdenFS types conflicts
for those is not modeled (`_untyped`).

The `interrupted` cell names a checkout known or chosen to have failed
following its parent/start group. Known pre-start failures preserve prior
state and have no parent/path groups. Unreported failures may name themselves
later through `_interruption_groups`. Only a same-target checkout can resume;
starting it ends the prior interruption.

A checkout the guard rejected records its would-be groups as
indeterminate, each requiring the impossible under the attempt's
`checkout.in-progress-rejection` obligation (`_rejected_groups`), and
one observation of what its message says (`rejection_observation`): the
interrupted advice needs an interrupted checkout to another target, the
in-progress text another checkout holding the guard. A failed status
says something about state too (`_status_observation`).

Checkout semantics, as EdenFS and Sapling behave on a live mount:

- A path is dirty when its bytes or its executable bit differ from the
  source entry (EdenFS compares the entry type).
- Thrift NORMAL applies a path only when it is clean or already equals
  the destination, and ignores Sapling's tracking.
- `sl goto` runs as Sapling runs it (`edenfs.rs:366-426`): a
  DRY_RUN visit per changed path marks whether its state blocks
  (`blocked`, per candidate source) and whether Sapling applies the path
  itself afterwards (`planned`: track an untracked copy of the
  destination); each NORMAL apply point uses
  the Thrift NORMAL table or the plan (read at the apply point when the
  visit has not run, so no order between them is needed) and, when the
  command succeeded, requires its visit to have run unblocked. A path blocks when
  it is dirty other than to the destination, untracked or added with
  other content, or absent (plainly unlinked or removed through
  Sapling) while the destination changes it. Any local change of a path
  the destination changes blocks; a dirty path it does not change is
  carried. An operation landing between a path's visit and its apply
  point is kept, with no abort.
- An aborting `sl goto` lists only paths that blocked
  (`abort_observations`).
  Every path an abort lists is relevant (`_context`), so its state is
  derived even when no action names it (a resume lists directories the
  interrupted walk removed).
- `.hg/updatestate` is a cell (`_updatestate_groups`): every `sl`
  checkout attempt deletes it, one that failed after DRY_RUN may leave
  its destination there, and `--continue` proceeds only when it names
  the continue's destination.
  The contract assumes Rust `cmdgoto`: its Python fallback (an
  unresolvable destination or mutation successors) starts only after
  that deletion.
- FORCE resets bytes and mode of every path either revision holds.
  `sl goto --clean` is FORCE without a DRY_RUN (`edenfs_force_checkout`);
  Sapling then forgets every path's tracking (`clear_edenfs_dirstate`),
  moves its parent like any `sl goto`, and never writes `.hg/updatestate`.
- A write or chmod overlapping a checkout may be superseded by it: it
  resolved the old inode before the swap and landed after it, so
  the cell it set ends at the destination's, with no conflict. An
  optional group per op, after the checkout has applied the path, resets
  only that op's cell; it exists wherever the checkout swaps the inode:
  under FORCE on every path either revision holds, changed or kept (the
  reported conflict is then optional), under NORMAL and `sl goto` on a
  path the destination changes.
- A path whose type changes takes its relevant descendants along: a new
  directory arrives with its children, and an old one is replaced only
  once its children are clean or gone. NORMAL keeps a directory with
  dirty or untracked children (DIRECTORY_NOT_EMPTY); FORCE also removes
  local-only children (`experimental:force-checkout-removes-local-only`,
  default true), which stays unencoded while validation refuses a path
  below a type-changing register. A directory replaced by a file is
  removed and the file bound in a second group, so the name may be
  absent in between (`_path_group`).

Interrupted checkouts apply each path independently: the per-directory
batch correlations EdenFS shows are not encoded, a sound
over-approximation. An interrupted `sl goto` may also apply its
DRY_RUN plan although Sapling's VFS step runs only after a successful
NORMAL checkout, another over-approximation.
"""

from __future__ import annotations

import errno
import re
from collections.abc import Callable, Iterable, Iterator, Mapping
from dataclasses import dataclass, replace

from eden.integration.correctness.exploratory.core.effects import (
    ABSENT,
    ANY,
    Atom,
    Before,
    Cell,
    Count,
    Effect,
    EffectGroup,
    EffectKind,
    eq,
    Expect,
    Guard,
    none_of,
    normalize_path,
    Observation,
    one_of,
    PinnedFacts,
    PRIMARY,
    Requirement,
    ROOT,
    Status,
    Value,
    Window,
)
from eden.integration.correctness.exploratory.core.facts import (
    ActionFacts,
    ErrnoUse,
    HistoryFacts,
    NO_ERRNO,
    Outcome,
    ResultSchema,
)
from eden.integration.correctness.exploratory.core.runspec import Action, RunSpec
from eden.integration.correctness.exploratory.edenfs.actions import (
    CHECKOUT_CONTINUE,
    CHECKOUT_FORCE,
    MOVE_KINDS,
)
from eden.integration.correctness.exploratory.edenfs.fixture import CHURN_FILES
from eden.integration.correctness.exploratory.edenfs.sites import CHECKOUT_BEGIN_SITE

from .files import (
    _chmod_value,
    _conjoined,
    _fault_caused,
    _meaning_rows,
    _move_pairs,
    _out_of_date_rows,
    _reachable,
    _split_on_value,
    _STATE_MEANINGS,
    _transfer,
    _UNOPENED,
    _unopened_observation,
    binding_cell,
    descriptor_cell,
    descriptor_keys,
    DESCRIPTOR_RULE,
    DescriptorGap,
    failure_errno_name,
    NONEMPTY_DESTINATION_ERRNOS,
    nonempty_destination_rows,
    object_cell,
    OPENED,
    PRECONDITION_RULE,
    process_failure_observation,
    write_value,
)
from .model import (
    _ABSENT_LOCAL,
    _AGGREGATE_KINDS,
    _ancestors,
    _ANY_LOCAL,
    _applies,
    _changes,
    _complement,
    _Context,
    _descendants,
    _dirt_outside,
    _eden_pinned,
    _entry,
    _Entry,
    _errno_name,
    _errno_of,
    _field,
    _FUSE_INDETERMINATE_ERRNOS,
    _group,
    _interval,
    _Local,
    _local_cells,
    _local_select,
    _marks,
    _moved,
    _Op,
    _OUT_OF_DATE_PARENT,
    _parent_path,
    _PARENT_READERS,
    _PATH_FIELDS,
    _path_part,
    _Plan,
    _post_effect_fault,
    _produce,
    _rank_keys,
    _replaces,
    _Resume,
    _retypes,
    _sapling_moves,
    _Select,
    _single,
    _stderr_first_line,
    _Structure,
    _unmarked,
    _Values,
    blocked_cell,
    BOUND,
    checkout_keys,
    CLEAN,
    CONFLICT,
    DIR,
    DRYRUN,
    EdenPinned,
    FUSE_MUTATIONS,
    GETATTR_ABSENCE_RULE,
    INPUT_LEGALITY_RULE,
    INTERRUPTED,
    KILLED,
    mode_cell,
    NEVER,
    parent_cell,
    PATH,
    path_cell,
    PLANNED,
    Resolution,
    resolution_boundary,
    rev_value,
    SAPLING_PARENT,
    source_cell,
    track_cell,
    UPDATESTATE,
    visit_cell,
    YES,
)


CHECKOUT_ORDER_RULE = "checkout.parent-before-paths"


CHECKOUT_ABORT_RULE = "checkout.sl-abort"


# The part of an aborting `sl goto`'s `checkout.sl-abort` scope
# (`abort_scope`): its per-path listing observations.
ABORT_LISTED = "listed"


# The `blockers` identity of each `checkout.sl-abort` scope part: a listed
# path in a state that does not block, or, for inputs scoped by the action
# alone (a successful update's changed paths, a `--continue`'s
# interrupted-goto state), no listed path.
ABORT_BLOCKERS: Mapping[str, str] = {"": "unlisted", ABORT_LISTED: "not-blocking"}


CONFLICT_COUNT_RULE = "checkout.conflict-count"


IN_PROGRESS_RULE = "checkout.in-progress-rejection"


INTERRUPTED_REFUSAL_RULE = "checkout.interrupted-refusal"


CONTRACT_RULES = frozenset(
    {
        DESCRIPTOR_RULE,
        INPUT_LEGALITY_RULE,
        PRECONDITION_RULE,
        CHECKOUT_ORDER_RULE,
        CHECKOUT_ABORT_RULE,
        CONFLICT_COUNT_RULE,
        IN_PROGRESS_RULE,
        INTERRUPTED_REFUSAL_RULE,
    }
)


CHECKOUT_IN_PROGRESS = "CHECKOUT_IN_PROGRESS"


_IN_PROGRESS_TEXT = "another checkout operation is still in progress"


_INTERRUPTED_TEXT = "a previous checkout was interrupted"


# Sapling surfaces EdenFS's guard rejection as `abort: EdenError: <message>`
# (for example `abort: EdenError: a previous checkout was interrupted`).
_SL_GUARD_ABORT = re.compile(
    rf"^abort: EdenError: ({re.escape(_IN_PROGRESS_TEXT)}|{re.escape(_INTERRUPTED_TEXT)})"
)


# Sapling's `set_parents` after the walk, refused by `EdenMount::resetParent`
# while another checkout holds the guard (fs/inodes/EdenMount.cpp:2654-2662):
# the walk itself completed.
_SL_SET_PARENTS_REFUSAL = "abort: EdenError: cannot reset parent while a checkout"


# `sl go --continue` without `.hg/updatestate`
# (`scm/lib/commands/commands/cmdgoto/src/lib.rs:163`).
_SL_NOT_INTERRUPTED = re.compile(r"^abort: not in an interrupted goto state")


# `EdenMount::co_diff` refusing a status while a checkout runs
# (fs/inodes/EdenMount.cpp); the in-progress text above is
# `beginCheckout` rejecting a second checkout.
_STATUS_IN_PROGRESS_TEXT = (
    "cannot compute status while a checkout is currently in progress"
)


_SL_CONFLICT_ABORT = re.compile(r"^abort: \d+ conflicting file changes:")


# Sapling lists at most five conflicting paths, then this line
# (scm/lib/checkout/src/lib.rs:1490-1508).
_SL_MORE_CONFLICTS = re.compile(r"^\.\.\.and \d+ more$")


def dryrun_cell(checkout: str, path: str, side: str = PRIMARY) -> Cell:
    """Return the cell an `sl goto`'s DRY_RUN visit of `path` marks."""
    return Cell(side, DRYRUN, f"{checkout}/{normalize_path(path)}")


def planned_cell(checkout: str, revision: int, path: str, side: str = PRIMARY) -> Cell:
    """Return the cell a DRY_RUN visit marks when Sapling applies `path` itself (from `revision`)."""
    return Cell(side, PLANNED, f"{checkout}/r{revision}/{normalize_path(path)}")


def updatestate_cell(side: str = PRIMARY) -> Cell:
    """Return the cell holding the destination `.hg/updatestate` names, or `absent`."""
    return Cell(side, UPDATESTATE, ROOT)


def interrupted_cell(side: str = PRIMARY) -> Cell:
    """Return the cell naming the checkout EdenFS holds interrupted, or `absent`."""
    return Cell(side, INTERRUPTED, ROOT)


def interrupted_value(checkout: str) -> Value:
    """Return the `interrupted` value naming one checkout by its `checkout_keys` key."""
    return Value("checkout", (checkout,))


def conflict_cell(checkout: str, path: str, side: str = PRIMARY) -> Cell:
    """Return the cell a Thrift checkout's per-path group marks when it reports `path` conflicting.

    `checkout` is the checkout's key from `checkout_keys`, not its action ID.
    """
    return Cell(side, CONFLICT, f"{checkout}/{normalize_path(path)}")


def is_in_progress_rejection(facts: ActionFacts) -> bool:
    """Return whether a checkout failed because EdenFS's checkout guard refused it.

    Thrift reports `error_type=CHECKOUT_IN_PROGRESS`; `sl goto` exits
    with `abort: EdenError: another checkout operation is still in
    progress` or `abort: EdenError: a previous checkout was interrupted`
    (another target while one is interrupted).
    """
    if facts.kind != "checkout" or facts.outcome != Outcome.FAIL:
        return False
    if _field(facts, "error_type") == CHECKOUT_IN_PROGRESS:
        return True
    return _SL_GUARD_ABORT.match(_stderr_first_line(facts)) is not None


def is_not_interrupted(facts: ActionFacts) -> bool:
    """Return whether `sl go --continue` found no interrupted goto (no `.hg/updatestate`)."""
    return (
        facts.kind == "checkout"
        and facts.outcome == Outcome.FAIL
        and facts.payload.get("mode") == CHECKOUT_CONTINUE
        and _SL_NOT_INTERRUPTED.match(_stderr_first_line(facts)) is not None
    )


def is_conflict_abort(facts: ActionFacts) -> bool:
    """Return whether `sl goto` aborted on conflicting local changes."""
    return (
        facts.kind == "checkout"
        and facts.outcome == Outcome.FAIL
        and _SL_CONFLICT_ABORT.match(_stderr_first_line(facts)) is not None
    )


def conflict_paths(facts: ActionFacts) -> tuple[str, ...]:
    """Return the paths an aborting `sl goto` lists, normalized, in order.

    The list ends at the advice line, or at the `...and N more` line that
    truncates it after five paths; the unlisted paths are unknown.
    """
    stderr = _field(facts, "stderr")
    if not is_conflict_abort(facts) or not isinstance(stderr, str):
        return ()
    paths = []
    for line in stderr.strip().splitlines()[1:]:
        text = line.strip()
        if not text or text.startswith("(") or _SL_MORE_CONFLICTS.match(text):
            break
        paths.append(normalize_path(text))
    return tuple(paths)


def _checkout_resolution(facts: ActionFacts) -> Resolution:
    """Classify a failed checkout: a typed rejection is `none`, anything else open.

    A rejected or conflict-aborted attempt changed nothing; so did a
    `--continue` that found no interrupted goto. Any other failure (an
    EdenError mid-walk, a transport error, a killed `sl`, an `sl goto`
    that gave up waiting for Sapling's lock) may leave the daemon
    applying the checkout.
    """
    if (
        is_in_progress_rejection(facts)
        or is_conflict_abort(facts)
        or is_not_interrupted(facts)
    ):
        return Resolution.NONE
    return Resolution.INDETERMINATE


def resolution(facts: ActionFacts, history: HistoryFacts | None = None) -> Resolution:
    """Return what one action's recorded result says about its effects.

    With `history`, a failure that an injected error at a `post` site may
    have caused is indeterminate (`_post_effect_fault`: an error injected
    at journal-record fails a write with EIO after its bytes landed).
    """
    if facts.invoked is None:
        return Resolution.NONE
    if facts.outcome is None or facts.outcome == Outcome.INFO:
        return Resolution.INDETERMINATE
    if facts.result is None:
        return Resolution.INDETERMINATE
    if facts.outcome == Outcome.OK:
        return Resolution.ACKED
    if facts.kind == "checkout":
        if history is not None and known_prestart(history, facts):
            return Resolution.NONE
        return _checkout_resolution(facts)
    if _post_effect_fault(facts, history):
        return Resolution.INDETERMINATE
    if facts.kind in FUSE_MUTATIONS:
        lost = _errno_of(facts) in _FUSE_INDETERMINATE_ERRNOS
        return (
            Resolution.INDETERMINATE
            if lost or _field(facts, "close_errno") is not None
            else Resolution.NONE
        )
    if facts.kind in ("sl_rm", "sl_mv") and _field(facts, "returncode") == KILLED:
        return Resolution.INDETERMINATE
    return Resolution.NONE


def effect_window(facts: ActionFacts, status: Resolution, boundary: float) -> Window:
    """Return the window an action's effects lie in.

    An acked effect lies inside the invocation-to-completion interval,
    and so does every effect of a checkout whose failure EdenFS reported
    (`_reported_failure`: the daemon replies only once its checkout
    ended); any other indeterminate one between the invocation and the
    resolution boundary, which is never before the action's own
    completion.
    """
    assert facts.invoked is not None
    if status == Resolution.ACKED or _reported_failure(facts):
        assert facts.completed is not None
        return Window(facts.invoked, facts.completed)
    end = max(
        boundary, facts.completed if facts.completed is not None else facts.invoked
    )
    return Window(facts.invoked, end)


def named_paths(spec: RunSpec) -> frozenset[str]:
    """Return every normalized path an action payload names."""
    return frozenset(
        normalize_path(action.payload[name])
        for action in spec.actions
        for name in _PATH_FIELDS
        if isinstance(action.payload.get(name), str)
    )


def checkout_revisions(spec: RunSpec, pinned: EdenPinned) -> tuple[int, ...]:
    """Return every revision the EdenFS parent can hold: the start and each target."""
    targets = {
        action.payload["revision"]
        for action in spec.actions
        if action.kind == "checkout"
    }
    return tuple(sorted(targets | {pinned.initial_revision}))


def listed_paths(facts: HistoryFacts) -> frozenset[str]:
    """Return every path an aborting `sl goto` lists, or an `ok` status lists
    as an entry or a diff path (normalized)."""
    listed = {
        path
        for action in facts.actions_of("checkout")
        for path in conflict_paths(action)
    }
    for status in facts.actions_of("status"):
        if status.outcome != Outcome.OK or status.result is None:
            continue
        listed |= {entry["path"] for entry in status.result.get("status")}
        listed |= set(status.result.get("diff_paths") or ())
    return frozenset(map(normalize_path, listed))


def relevant_paths(
    spec: RunSpec, pinned: EdenPinned, listed: frozenset[str] = frozenset()
) -> tuple[str, ...]:
    """Return the paths checkout groups are derived for, without enumerating the fixture.

    They are the paths actions name, the paths in `listed` (the contracts
    passes `listed_paths`, so a listed path's state is derived even when
    no action names it), the reserved paths, type-changed paths and their
    children, where moves carry any of those (`_carried`), and, when some
    aggregate observation covers untouched paths, one representative per
    equivalence class of the rest.
    """
    fixture = pinned.fixture
    layout = fixture.layout
    named = named_paths(spec) | listed
    paths = set(named) | set(fixture.registers) | set(CHURN_FILES)
    for change in layout.type_changes:
        paths |= {change.path, layout.type_change_child(change.path).path}
    if any(action.kind in _AGGREGATE_KINDS for action in spec.actions):
        revisions = checkout_revisions(spec, pinned)
        paths |= {item.representative for item in fixture.classes(revisions, named)}
        if layout.wide_dir is not None:
            paths.add(next(layout.wide_files()).path)
    return tuple(sorted(_carried(spec, {normalize_path(path) for path in paths})))


def _carried(spec: RunSpec, paths: set[str]) -> set[str]:
    """Return `paths` plus where moves can carry every path under their sources.

    A move carries the relevant paths under its source to the same paths
    under its destination. Each move runs once, so a chain of moves is at
    most as long as the spec has moves.
    """
    moves = [
        (normalize_path(action.payload["source"]) + "/", action.payload["destination"])
        for action in spec.actions
        if action.kind in MOVE_KINDS
    ]
    for _ in moves:
        paths |= {
            normalize_path(f"{destination}/{path[len(source) :]}")
            for path in paths
            for source, destination in moves
            if path.startswith(source)
        }
    return paths


def class_weights(
    spec: RunSpec, pinned: EdenPinned, listed: frozenset[str] = frozenset()
) -> Mapping[str, int]:
    """Return how many untouched generated paths each relevant class representative stands for.

    Empty when no aggregate observation makes the contracts expand classes
    (`relevant_paths`, with the same `listed`).
    """
    if not any(action.kind in _AGGREGATE_KINDS for action in spec.actions):
        return {}
    revisions = checkout_revisions(spec, pinned)
    classes = pinned.fixture.classes(revisions, named_paths(spec) | listed)
    return {normalize_path(item.representative): item.count for item in classes}


def _ops(
    spec: RunSpec, facts: HistoryFacts, boundary: float
) -> dict[str, tuple[_Op, ...]]:
    """Return each path's writes and chmods that may have set their cell, with windows."""
    found: dict[str, list[_Op]] = {}
    keys = _rank_keys(facts, ("write", "chmod"), "o")
    for action in spec.actions:
        if action.kind not in ("write", "chmod"):
            continue
        recorded = facts.action(action.id)
        status = resolution(recorded, facts)
        if status == Resolution.NONE:
            continue
        path = normalize_path(action.payload["path"])
        window = effect_window(recorded, status, boundary)
        if action.kind == "write":
            op = _Op(
                action.id,
                keys[action.id],
                path_cell(path),
                write_value(spec, action),
                window,
            )
        else:
            op = _Op(
                action.id,
                keys[action.id],
                mode_cell(path),
                _chmod_value(action),
                window,
            )
        found.setdefault(path, []).append(op)
    return {path: tuple(items) for path, items in found.items()}


def _context(
    spec: RunSpec, facts: HistoryFacts, pinned: EdenPinned, rules: frozenset[str]
) -> _Context:
    boundary = resolution_boundary(facts)
    listed = listed_paths(facts)
    relevant = relevant_paths(spec, pinned, listed)
    return _Context(
        spec=spec,
        facts=facts,
        pinned=pinned,
        boundary=boundary,
        rules=rules,
        relevant=relevant,
        relevant_set=frozenset(relevant),
        candidates=checkout_revisions(spec, pinned),
        ops=_ops(spec, facts, boundary),
        checkout_keys=checkout_keys(facts),
        plans=_plans(spec, facts, pinned, boundary),
        weights=class_weights(spec, pinned, listed),
        named=named_paths(spec) | listed,
        interruptible=any(map(_may_interrupt, facts.actions_of("checkout"))),
    )


def _parent_window(
    facts: ActionFacts, boundary: float, history: HistoryFacts
) -> Window | None:
    """Return where a checkout's parent group may lie, or None when it has none.

    A typed rejection records its would-be groups inside its own interval.
    """
    if facts.invoked is None:
        return None
    if is_in_progress_rejection(facts):
        return (
            None if facts.completed is None else Window(facts.invoked, facts.completed)
        )
    status = resolution(facts, history)
    if status == Resolution.NONE:
        return None
    return effect_window(facts, status, boundary)


def _start_value(
    window: Window, before: list[tuple[ActionFacts, Window]], initial: int
) -> int | None:
    """Return the parent at `window.lo` when the moves `before` fix it, else None.

    The last move must be acked and start after every other one ended.
    """
    if not before:
        return initial
    last, last_window = max(before, key=lambda item: item[1].lo)
    if resolution(last) != Resolution.ACKED:
        return None
    if any(other is not last and w.hi >= last_window.lo for other, w in before):
        return None
    return last.payload["revision"]


def _plans(
    spec: RunSpec, facts: HistoryFacts, pinned: EdenPinned, boundary: float
) -> dict[str, _Plan]:
    """Return each invoked checkout's `_Plan`."""
    checkouts = facts.actions_of("checkout")
    movers = [
        (item, window)
        for item in checkouts
        if (window := _parent_window(item, boundary, facts)) is not None
    ]
    observers = [
        window
        for action in spec.actions
        if action.kind in _AGGREGATE_KINDS or action.kind in _PARENT_READERS
        if (window := _interval(facts.action(action.id), boundary)) is not None
    ]
    keys = checkout_keys(facts)
    plans: dict[str, _Plan] = {}
    for checkout in checkouts:
        own = _parent_window(checkout, boundary, facts) or _interval(checkout, boundary)
        if own is None:
            continue
        others = [(item, w) for item, w in movers if item.id != checkout.id]
        serialized = any(w.meets(own) for _item, w in others)
        resumes = tuple(
            _Resume(keys[other.id], None)
            for other in checkouts
            if other.id != checkout.id
            and other.payload["revision"] == checkout.payload["revision"]
            and other.invoked is not None
            and other.invoked <= own.hi
            and _may_interrupt(other, facts)
        )
        settled = None
        # Whether an indeterminate checkout moved the parent at all stays
        # open after its window, so every later observer watches it.
        seen = (
            Window(own.lo, max(own.hi, boundary))
            if _may_interrupt(checkout, facts)
            else own
        )
        watched = sum(w.meets(seen) for w in observers) > (
            1 if _interval(checkout, boundary) is not None else 0
        )
        if not serialized and not resumes and not watched:
            settled = _start_value(
                own,
                [(item, w) for item, w in others if w.hi < own.lo],
                pinned.initial_revision,
            )
        plans[checkout.id] = _Plan(settled, serialized, resumes)
    for checkout_id, plan in plans.items():
        plans[checkout_id] = replace(
            plan,
            resumes=tuple(
                _Resume(
                    resume.key,
                    next(
                        plans[other].settled
                        for other, key in keys.items()
                        if key == resume.key and other in plans
                    ),
                )
                for resume in plan.resumes
            ),
        )
    return plans


@dataclass(frozen=True)
class _Checkout:
    """One checkout's static description.

    `sources` are the revisions its per-path conditions enumerate: the
    settled one, or every candidate (named by the source marks).
    An `sl` checkout runs as Sapling runs it: per-path DRY_RUN visits,
    then per-path NORMAL applies (`edenfs.rs:366-426`); `sl goto --clean`
    is not one: it runs FORCE alone.
    """

    action: Action
    key: str
    target: int
    force: bool
    sl: bool
    status: Status
    window: Window
    acked: bool
    sources: tuple[int, ...]
    settled: bool
    serialized: bool
    resumes: tuple[_Resume, ...]
    counting: bool = False


def _checkout_spec(
    ctx: _Context,
    action: Action,
    status: Status,
    window: Window,
    *,
    counting: bool = False,
) -> _Checkout:
    payload = action.payload
    plan = ctx.plans.get(action.id, _Plan(None, True, ()))
    return _Checkout(
        action=action,
        key=ctx.checkout_keys[action.id],
        target=payload["revision"],
        force=payload["mode"] == CHECKOUT_FORCE,
        sl=payload["via"] != "thrift" and payload["mode"] != CHECKOUT_FORCE,
        status=status,
        window=window,
        acked=status == Status.ACKED,
        sources=ctx.candidates if plan.settled is None else (plan.settled,),
        settled=plan.settled is not None,
        serialized=plan.serialized,
        resumes=plan.resumes,
        counting=counting,
    )


def _sl_passes(source: _Entry, target: _Entry) -> list[_Local] | None:
    """Return the local states that do not abort an `sl goto`, or None for any.

    A changed path blocks when it is modified (bytes or executable bit)
    other than to the destination, untracked or added with other content,
    or absent: plainly unlinked (`!`) or removed through Sapling (`sl rm`,
    `sl mv` source). Sapling's contract treats a missing path the
    destination changes as a local modification (`lib.rs:431-441`).
    Sapling itself exempts an untracked copy whose bytes equal the
    destination whatever its mode, and never an added one
    (`lib.rs:431-453`); this encoding compares the full local state, a
    difference generation never reaches.
    """
    if not _changes(source, target):
        return None
    if source.value == ABSENT:
        return [_ABSENT_LOCAL, target.local()]
    if target.value == ABSENT:
        return [source.local()]
    return [source.local(), target.local()]


def _sl_plan(source: _Entry, target: _Entry) -> list[_Local]:
    """Return the DRY_RUN states whose path Sapling sets to the destination itself.

    After the NORMAL checkout Sapling applies the DRY_RUN plan through its
    VFS (`edenfs.rs:419`): an untracked copy of the destination becomes
    tracked (UNTRACKED_ADDED).
    """
    if _changes(source, target) and source.value == ABSENT:
        return [target.local()]
    return []


def _source_cells(ctx: _Context, checkout: _Checkout) -> tuple[Cell, ...]:
    if checkout.settled:
        return ()
    return tuple(source_cell(checkout.key, revision) for revision in ctx.candidates)


def _sources(
    ctx: _Context, checkout: _Checkout, revision: int
) -> list[tuple[Atom, ...]]:
    """Return the atom tuples over `_source_cells` naming `revision` as the source.

    A settled checkout reads no mark: its one source is a constant. Else
    the mark names the source once the parent group ran. Before that no
    mark is set, and every candidate's rows apply: the order requirement
    (`_order_requirement`) keeps every legal explanation from applying a
    path then; an explanation without it (a deletion trial) still applies
    the path from whichever revision it holds, never as a no-op, so
    dropping the order explains no stale path and
    `checkout.parent-before-paths` joins a proof only when the
    order itself is contradicted. No row reads the EdenFS parent.
    """
    if checkout.settled:
        return [()]
    return [_marks(ctx, revision), _unmarked(ctx)]


def _by_source(
    ctx: _Context,
    checkout: _Checkout,
    path: str,
    select: _Select,
    *,
    extra_cells: tuple[Cell, ...] = (),
    structure: _Structure | None = None,
    skip: frozenset[int] = frozenset(),
) -> tuple[tuple[Cell, ...], Expect] | None:
    """Return the cells and rows of a per-path condition, one block per source revision.

    The cells are the path's local cells, `extra_cells`, the structure's
    cells, and the source marks (`_source_cells`). Revisions in `skip`
    contribute no row. Returns None when no source revision has a
    satisfying state.
    """
    target = _entry(ctx.pinned, path, checkout.target)
    extra = _Structure() if structure is None else structure
    width = len(_ANY_LOCAL) + len(extra_cells)
    rows = []
    for revision in checkout.sources:
        if revision in skip:
            continue
        source = _entry(ctx.pinned, path, revision)
        selected = select(revision, source, target)
        locals_ = [tuple(ANY for _ in range(width))] if selected is None else selected
        sources = _sources(ctx, checkout, revision)
        for tail in extra.tails(ctx, revision, source, target):
            rows.extend(
                (*local, *tail, *marks) for local in locals_ for marks in sources
            )
    if not rows:
        return None
    cells = (
        *_local_cells(path),
        *extra_cells,
        *extra.cells(),
        *_source_cells(ctx, checkout),
    )
    return cells, Expect(tuple(dict.fromkeys(rows)))


def _changing(ctx: _Context, checkout: _Checkout, path: str) -> list[int]:
    """Return the source revisions from which the destination changes `path`."""
    target = _entry(ctx.pinned, path, checkout.target)
    return [
        revision
        for revision in checkout.sources
        if _changes(_entry(ctx.pinned, path, revision), target)
    ]


def _phase_cells(ctx: _Context, checkout: _Checkout, path: str) -> tuple[Cell, ...]:
    """Return the DRY_RUN cells an `sl goto` apply point reads: its visit, then each plan."""
    changing = _changing(ctx, checkout, path) if checkout.sl else []
    if not changing:
        return ()
    return (
        dryrun_cell(checkout.key, path),
        *(planned_cell(checkout.key, revision, path) for revision in changing),
    )


def _path_select(ctx: _Context, checkout: _Checkout, path: str) -> _Select:
    """Return the states under which this checkout's apply point changes a path.

    FORCE resets every path the source or destination holds, whatever its
    local state, and leaves others untouched. NORMAL uses EdenFS's table
    (`_applies`), for Thrift and for `sl goto`'s NORMAL phase alike; an
    `sl goto` also sets the path to the destination when its DRY_RUN
    visit planned it (`_sl_plan`, read from the `planned` cell, over
    `_phase_cells`). Before its visit ran, the plan's states are read at
    the apply point itself, so an apply point never depends on lying after
    its visit: placing the visit just before it explains the same. An
    open `sl goto` may have stopped after the daemon's walk, which these
    rows cover.
    """
    if checkout.force:
        return _local_select(lambda s, t: [_ANY_LOCAL] if _replaces(True, s, t) else [])
    planned = _changing(ctx, checkout, path) if checkout.sl else []
    phase = (ANY, *(ANY for _ in planned)) if planned else ()

    def select(revision: int, source: _Entry, target: _Entry) -> list[tuple[Atom, ...]]:
        rows: list[tuple[Atom, ...]] = [
            (*local, *phase) for local in _applies(source, target)
        ]
        if revision in planned:
            marks = tuple(eq(YES) if r == revision else ANY for r in planned)
            rows.append((*_ANY_LOCAL, ANY, *marks))
            rows += [
                (*local, eq(ABSENT), *(ANY for _ in planned))
                for local in _sl_plan(source, target)
            ]
        return rows

    return select


def _order_requirement(ctx: _Context, checkout: _Checkout) -> tuple[Requirement, ...]:
    """An apply point follows its own parent group.

    Some source mark, which only this checkout's parent group sets, is
    set; that carries the order even when the destination is the
    revision the checkout starts from. When another checkout's parent
    move may land inside this window (`serialized`), the parent must also
    still be at the destination, so no path of an earlier checkout
    follows a later one's parent move. A settled checkout needs no mark:
    nothing reads the parent inside its window, so where its parent group
    lies among its own groups decides nothing (`_plans`).
    """
    ref = ctx.obligation(CHECKOUT_ORDER_RULE, checkout.action.id)
    if ref is None:
        return ()
    cells: list[Cell] = list(_source_cells(ctx, checkout))
    rows: list[tuple[Atom, ...]] = (
        [()] if checkout.settled else [_marks(ctx, r) for r in ctx.candidates]
    )
    if checkout.serialized:
        cells.append(parent_cell())
        rows = [(*row, eq(rev_value(checkout.target))) for row in rows]
    if not cells:
        return ()
    return (Requirement(tuple(cells), Expect(tuple(rows)), ref),)


def _abort_requirement(
    ctx: _Context, checkout: _Checkout, path: str
) -> tuple[Requirement, ...]:
    """A successful `sl goto` found no blocking state at a changed path's DRY_RUN visit.

    The visit marks `blocked` per candidate source; the apply point
    requires the visit to have run and the mark of its own source absent.
    """
    ref = ctx.obligation(CHECKOUT_ABORT_RULE, checkout.action.id)
    if ref is None or not (checkout.sl and checkout.acked):
        return ()
    blocking = _blocking(ctx, checkout, path)
    if not blocking:
        return ()
    cells = (
        dryrun_cell(checkout.key, path),
        *(blocked_cell(checkout.key, r, path) for r in blocking),
        *_source_cells(ctx, checkout),
    )
    rows = tuple(
        (eq(YES), *(eq(ABSENT) if r == revision else ANY for r in blocking), *marks)
        for revision in checkout.sources
        for marks in _sources(ctx, checkout, revision)
    )
    return (Requirement(cells, Expect(tuple(dict.fromkeys(rows))), ref),)


def _blocking(ctx: _Context, checkout: _Checkout, path: str) -> list[int]:
    """Return the source revisions from which some state of `path` aborts `sl goto`."""
    target = _entry(ctx.pinned, path, checkout.target)
    found = []
    for revision in checkout.sources:
        passes = _sl_passes(_entry(ctx.pinned, path, revision), target)
        if passes is not None and _complement(passes):
            found.append(revision)
    return found


def _dryrun_group(ctx: _Context, checkout: _Checkout, path: str) -> EffectGroup | None:
    """`sl goto`'s DRY_RUN visit of one changed path (`edenfs.rs:381-396`).

    It reads the path's local state at its own point and records, per
    candidate source, whether that state blocks (`blocked`, read by the
    abort requirement when the command succeeded) and whether Sapling
    applies the path itself after the NORMAL phase (`planned`). It
    changes nothing visible, so racing operations between it and the
    path's NORMAL apply point are kept.
    """
    target = _entry(ctx.pinned, path, checkout.target)
    changing = _changing(ctx, checkout, path)
    if not changing:
        return None
    action = checkout.action.id
    effects = [_produce(dryrun_cell(checkout.key, path), YES, action)]
    cells = _local_cells(path)
    for revision in _blocking(ctx, checkout, path) if checkout.acked else []:
        passes = _sl_passes(_entry(ctx.pinned, path, revision), target)
        assert passes is not None
        guard = Guard(cells, Expect(tuple(_complement(passes))))
        effects.append(
            _produce(blocked_cell(checkout.key, revision, path), YES, action, guard)
        )
    for revision in changing:
        plan = _sl_plan(_entry(ctx.pinned, path, revision), target)
        guard = Guard(cells, Expect(tuple(plan) or ((NEVER, ANY, ANY),)))
        effects.append(
            _produce(planned_cell(checkout.key, revision, path), YES, action, guard)
        )
    return _group(
        checkout.action,
        checkout.status,
        checkout.window,
        tuple(effects),
        part=f"{_path_part(path)}:dryrun",
    )


def _parent_group(ctx: _Context, checkout: _Checkout) -> EffectGroup:
    """Move the parent at CheckoutContext.start, before path effects.

    Pre-start failures restore prior state (`known_prestart`); ambiguous
    failures may omit this group. A reported failure after start names its
    interruption here; otherwise it clears the cell. Source marks guard
    per-path manifests when the checkout is not settled.
    """
    action = checkout.action
    marks = []
    if not checkout.settled:
        cells, choices = _source_choices(ctx, action)
        for revision in ctx.candidates:
            rows = tuple(dict.fromkeys(row for r, row in choices if r == revision))
            guard = Guard(cells, Expect(rows))
            marks.append(
                _produce(source_cell(checkout.key, revision), YES, action.id, guard)
            )
    interruption = (
        interrupted_value(checkout.key)
        if _reported_failure(ctx.facts.action(action.id))
        else ABSENT
    )
    clear = (
        (_produce(interrupted_cell(), interruption, action.id),)
        if ctx.interruptible
        else ()
    )
    return _group(
        action,
        checkout.status,
        checkout.window,
        (
            _produce(parent_cell(), rev_value(checkout.target), action.id),
            *clear,
            *marks,
        ),
        part="parent",
        requires=_guard_passed(ctx, action),
    )


def _guard_passed(ctx: _Context, action: Action) -> tuple[Requirement, ...]:
    """Getting past the guard needs no interrupted checkout to another target
    (`beginCheckout`, EdenMount.cpp:1757-1783)."""
    ref = ctx.obligation(INTERRUPTED_REFUSAL_RULE, action.id)
    target = action.payload["revision"]
    others = {
        interrupted_value(ctx.checkout_keys[other.id])
        for other in ctx.facts.actions_of("checkout")
        if other.payload["revision"] != target
        and _reported_failure(other)
        and _may_interrupt(other, ctx.facts)
    }
    if ref is None or not ctx.interruptible or not others:
        return ()
    return (Requirement((interrupted_cell(),), _single(none_of(others)), ref),)


def _destination_effects(
    ctx: _Context, checkout: _Checkout, path: str, guard: Guard, *, track: bool
) -> list[Effect]:
    action = checkout.action
    target = _entry(ctx.pinned, path, checkout.target)
    effects = [
        _produce(path_cell(path), target.value, action.id, guard),
        _produce(mode_cell(path), target.mode, action.id, guard),
    ]
    if track:
        effects.append(_produce(track_cell(path), CLEAN, action.id, guard))
    return effects


def _created_with_parent(
    ctx: _Context, checkout: _Checkout, path: str
) -> frozenset[int]:
    """Return the source revisions from which the checkout turns `path`'s parent into a directory.

    From those the path is created by its parent's group, together with
    the new directory (one entry swap).
    """
    parent = _parent_path(path)
    if parent == ROOT or parent not in ctx.relevant_set:
        return frozenset()
    target = _entry(ctx.pinned, parent, checkout.target)
    return frozenset(
        revision
        for revision in checkout.sources
        if target.value == DIR and _entry(ctx.pinned, parent, revision).value != DIR
    )


def _apply_guard(ctx: _Context, checkout: _Checkout, path: str) -> Guard | None:
    """Return the guard under which the checkout's apply point replaces `path`."""
    target = _entry(ctx.pinned, path, checkout.target)
    retyped = any(
        _retypes(_entry(ctx.pinned, path, revision), target)
        for revision in checkout.sources
    )
    built = _by_source(
        ctx,
        checkout,
        path,
        _path_select(ctx, checkout, path),
        extra_cells=_phase_cells(ctx, checkout, path),
        structure=_Structure(_descendants(ctx, path) if retyped else ()),
        skip=_created_with_parent(ctx, checkout, path),
    )
    return None if built is None else Guard(*built)


def _path_group(ctx: _Context, checkout: _Checkout, path: str) -> Iterator[EffectGroup]:
    """The checkout's apply point for one path: when its guard holds, replace it and mark it visited.

    A path whose type changes takes its relevant descendants along in the
    same group: a new directory arrives with its children, and an old one
    leaves only once its children are clean or gone. A directory replaced
    by a file is unbound first and the file bound later under a second
    contents lock (`TreeInode::finalizeDirectoryRemoval`), so the name may
    be absent in between: the apply point removes a directory it applies
    and marks it `removed`, and a second group binds the file after that
    removal while nothing bound the name meanwhile (EdenFS keeps a racing
    entry), after an apply point that applied to no directory, or, placed
    before the apply point or after one that did not apply, under the
    apply guard itself; every order ends at the destination.
    """
    guard = _apply_guard(ctx, checkout, path)
    if guard is None:
        return
    target = _entry(ctx.pinned, path, checkout.target)
    sources = [_entry(ctx.pinned, path, revision) for revision in checkout.sources]
    retyped = any(_retypes(source, target) for source in sources)
    track = checkout.sl and checkout.acked
    action = checkout.action.id
    visited = visit_cell(checkout.key, path)
    # The guard's rows where the path is a directory, and where it is not;
    # its first cell is the path's (`_by_source`).
    directories: list[tuple[Atom, ...]] = []
    others: list[tuple[Atom, ...]] = []
    for first, *rest in guard.expect.rows:
        for rows, inside in ((directories, True), (others, False)):
            narrowed = _Values.of(first).meet(_Values(inside, frozenset({DIR})))
            if not narrowed.empty:
                rows.append((narrowed.atom(), *rest))
    unbinds = (
        target.value not in (ABSENT, DIR)
        and any(source.value == DIR for source in sources)
        and bool(directories)
    )
    removed = visit_cell(f"{checkout.key}:removed", path)
    if unbinds:
        removal = Guard(guard.cells, Expect(tuple(directories)))
        effects = [
            _produce(path_cell(path), ABSENT, action, removal),
            _produce(mode_cell(path), ABSENT, action, removal),
            _produce(removed, YES, action, removal),
        ]
    else:
        effects = _destination_effects(ctx, checkout, path, guard, track=track)
    for child in _descendants(ctx, path) if retyped else ():
        effects += _destination_effects(ctx, checkout, child, guard, track=track)
    effects.append(_produce(visited, YES, action, guard))
    marker = _conflict_marker(ctx, checkout, path)
    if marker is not None:
        effects += [marker, *_nonempty_markers(ctx, checkout, path).values()]
    yield _group(
        checkout.action,
        checkout.status,
        checkout.window,
        tuple(effects),
        part=_path_part(path),
        requires=(
            *_order_requirement(ctx, checkout),
            *_abort_requirement(ctx, checkout, path),
        ),
    )
    if not unbinds:
        return
    free = tuple(ANY for _ in guard.cells[1:])
    rows = (
        (eq(YES), ANY, eq(ABSENT), *free),
        *((eq(ABSENT), eq(YES), *row) for row in others),
        *((eq(ABSENT), eq(ABSENT), *row) for row in guard.expect.rows),
    )
    insertion = Guard((removed, visited, *guard.cells), Expect(rows))
    yield _group(
        checkout.action,
        checkout.status,
        checkout.window,
        tuple(_destination_effects(ctx, checkout, path, insertion, track=track)),
        part=f"{_path_part(path)}:insert",
        requires=_order_requirement(ctx, checkout),
    )


def _supersede_groups(
    ctx: _Context, checkout: _Checkout, path: str
) -> Iterator[EffectGroup]:
    """A checkout may replace a path under an overlapping write or chmod.

    The op resolved the old inode (its open or lookup) before the
    checkout swapped the entry, so its effect lands in the unlinked inode:
    the cell it set ends at the destination's and the op is invisible.
    Every inode swap allows this (`_replaces`): FORCE on any path either
    revision holds, NORMAL and `sl goto` on a path the destination
    changes. That outcome is optional and per op, so each overlapping op
    gets its own indeterminate group, inside both windows, that resets
    only the cell it set (bytes for a write, mode for a chmod) once the
    checkout's own per-path group has applied the path; a write and a
    chmod racing on one path resolve independently. The group marks no
    conflict, so the op lands after the visit and the reported count
    leaves it out. A path the per-path group kept as dirty is never
    marked, so an op already visible before a NORMAL checkout is kept (or
    aborts `sl goto`). The guard reads only the op's cell, an
    over-approximation of the state the checkout saw before the op.
    An op that resolved the path only after the swap reached the new
    inode and is kept: a `resolve` point inside both
    windows marks the op only while the path is not yet visited, and the
    reset requires that mark.
    """
    for op in ctx.ops.get(path, ()):
        lo = max(op.window.lo, checkout.window.lo)
        hi = min(op.window.hi, checkout.window.hi)
        if lo > hi:
            continue
        visited = visit_cell(checkout.key, path)
        resolved = visit_cell(f"{checkout.key}:{op.key}", path)
        row: tuple[Atom, ...] = (
            (eq(op.value), ANY, ANY, eq(YES))
            if op.cell.space == PATH
            else (ANY, eq(op.value), ANY, eq(YES))
        )
        built = _by_source(
            ctx,
            checkout,
            path,
            _local_select(
                lambda s, t, row=row: [row] if _replaces(checkout.force, s, t) else []
            ),
            extra_cells=(resolved,),
            structure=_Structure(visited=visited),
        )
        if built is None:
            continue
        part = f"{_path_part(path)}:supersede:{op.action}"
        yield _group(
            checkout.action,
            Status.INDETERMINATE,
            Window(lo, hi),
            (
                _produce(
                    resolved,
                    YES,
                    checkout.action.id,
                    Guard((visited,), _single(eq(ABSENT))),
                ),
            ),
            part=f"{part}:resolve",
        )
        effects = _destination_effects(ctx, checkout, path, Guard(*built), track=False)
        yield _group(
            checkout.action,
            Status.INDETERMINATE,
            Window(lo, hi),
            tuple(effect for effect in effects if effect.cell == op.cell),
            part=part,
            requires=_order_requirement(ctx, checkout),
        )


def _track_group(ctx: _Context, checkout: _Checkout, path: str) -> EffectGroup | None:
    """An interrupted `sl goto` may or may not have reset a changed path's tracking."""
    guard = _apply_guard(ctx, checkout, path)
    if guard is None:
        return None
    action = checkout.action
    return _group(
        action,
        checkout.status,
        checkout.window,
        (_produce(track_cell(path), CLEAN, action.id, guard),),
        part=f"{_path_part(path)}:track",
        requires=_order_requirement(ctx, checkout),
    )


def _checkout_groups(
    ctx: _Context, action: Action, status: Status, window: Window
) -> Iterator[EffectGroup]:
    checkout = _checkout_spec(
        ctx, action, status, window, counting=_counting(ctx, action, status)
    )
    yield _parent_group(ctx, checkout)
    for path in ctx.relevant:
        if checkout.sl:
            dryrun = _dryrun_group(ctx, checkout, path)
            if dryrun is not None:
                yield dryrun
        yield from _path_group(ctx, checkout, path)
        yield from _supersede_groups(ctx, checkout, path)
        if checkout.sl and not checkout.acked:
            track = _track_group(ctx, checkout, path)
            if track is not None:
                yield track
    recorded = ctx.facts.action(action.id)
    if action.payload["via"] != "thrift" and _sapling_moves(recorded):
        yield _group(
            action,
            status,
            window,
            (
                _produce(
                    parent_cell(SAPLING_PARENT), rev_value(checkout.target), action.id
                ),
                # `set_parents` resets EdenFS's parent too, after the update's
                # own walk: it differs only if another checkout ran meanwhile.
                *(
                    (_produce(parent_cell(), rev_value(checkout.target), action.id),)
                    if checkout.serialized
                    else ()
                ),
                *(
                    _produce(track_cell(path), CLEAN, action.id)
                    for path in ctx.relevant
                    if checkout.force
                ),
            ),
            part="sapling",
            requires=_parents_reset(ctx, action),
        )


def _parents_reset(ctx: _Context, action: Action) -> tuple[Requirement, ...]:
    """`set_parents` needs no interrupted checkout (`resetParentCommits`
    refuses one, EdenMount.cpp:2691-2699)."""
    ref = ctx.obligation(INTERRUPTED_REFUSAL_RULE, action.id)
    if ref is None or not ctx.interruptible:
        return ()
    return (Requirement((interrupted_cell(),), _single(eq(ABSENT)), ref),)


def _updatestate_groups(ctx: _Context, action: Action) -> tuple[EffectGroup, ...]:
    """Return what one `sl` checkout attempt does to `.hg/updatestate`.

    Every attempt that took Sapling's lock deletes the file before any
    checkout (`cmdgoto` :172-174), whatever its target and outcome, except
    a `--continue` that found no file and bailed, and an attempt EdenFS's
    checkout guard refused (`is_in_progress_rejection`): a refused attempt
    keeps the file, so the interrupted update stays resumable with
    `--continue` (T294157075). An attempt that passed
    its DRY_RUN writes the destination there (`edenfs.rs:398-402`) and a
    successful one deletes it again; Sapling's lock serializes attempts,
    so only an attempt that failed after DRY_RUN (a guard refusal may
    come from its NORMAL phase), or whose outcome is unknown, may leave
    it written; `sl goto --clean` has no DRY_RUN and never writes it
    (`edenfs_force_checkout`). The file is Sapling's own, so an
    attempt that exited touched it only before it exited. A `--continue` proceeds only when the
    file names its destination: that requirement is the attempt's
    `checkout.sl-abort` input.
    """
    recorded = ctx.facts.action(action.id)
    if (
        action.payload["via"] == "thrift"
        or recorded.invoked is None
        or is_not_interrupted(recorded)
    ):
        return ()
    status = resolution(recorded)
    resolved = status == Resolution.ACKED or (
        status == Resolution.NONE and recorded.completed is not None
    )
    if (
        recorded.outcome in (Outcome.OK, Outcome.FAIL)
        and recorded.completed is not None
    ):
        window = Window(recorded.invoked, recorded.completed)
    else:
        window = effect_window(recorded, Resolution.INDETERMINATE, ctx.boundary)
    group_status = Status.ACKED if resolved else Status.INDETERMINATE
    destination = rev_value(action.payload["revision"])
    requires: tuple[Requirement, ...] = ()
    ref = ctx.obligation(CHECKOUT_ABORT_RULE, action.id)
    if action.payload["mode"] == CHECKOUT_CONTINUE and ref is not None:
        requires = (Requirement((updatestate_cell(),), _single(eq(destination)), ref),)
    refused = is_in_progress_rejection(recorded)
    keep = Guard((updatestate_cell(),), _single(NEVER)) if refused else None
    clear = _group(
        action,
        group_status,
        window,
        (_produce(updatestate_cell(), ABSENT, action.id, keep),),
        part="updatestate",
        requires=requires,
    )
    if action.payload["mode"] == CHECKOUT_FORCE or (resolved and not refused):
        return (clear,)
    written = _group(
        action,
        Status.INDETERMINATE,
        window,
        (_produce(updatestate_cell(), destination, action.id),),
        part="updatestate:written",
    )
    return (clear, written)


def continue_observation(ctx: _Context, action: Action) -> Observation | None:
    """Return what `--continue` failing "not in an interrupted goto state" says.

    It found no `.hg/updatestate`, at one point inside the call;
    that refusal is the attempt's `checkout.sl-abort` input.
    """
    recorded = ctx.facts.action(action.id)
    ref = ctx.obligation(CHECKOUT_ABORT_RULE, action.id)
    if ref is None or not is_not_interrupted(recorded) or recorded.completed is None:
        return None
    assert recorded.invoked is not None
    return Observation(
        id=f"o:{action.id}:updatestate",
        action=action.id,
        cells=(updatestate_cell(),),
        expect=_single(eq(ABSENT)),
        window=Window(recorded.invoked, recorded.completed),
        channel="sl",
        obligations=(ref,),
    )


def _conflict_states(force: bool) -> Callable[[_Entry, _Entry], list[_Local]]:
    """Return the local states a Thrift checkout reports as a conflict at a path.

    In both modes a path neither revision holds is never examined. A
    path the destination changes conflicts when it is present and the
    source lacks it (UNTRACKED_ADDED, even when equal to the destination),
    when it is absent and the source holds it (REMOVED_MODIFIED, or
    MISSING_REMOVED when the destination removes it too), and when it is
    present and equals neither entry (MODIFIED_MODIFIED, MODIFIED_REMOVED).
    FORCE also reports a path the destination leaves unchanged whenever
    it differs from that entry. Bytes and the executable bit both count;
    tracking does not.
    """

    def select(source: _Entry, target: _Entry) -> list[_Local]:
        if source.value == ABSENT and target.value == ABSENT:
            return []
        if not _changes(source, target):
            return _complement([source.local()]) if force else []
        if source.value == ABSENT:
            return [(none_of({ABSENT}), ANY, ANY)]
        keep = [source.local()]
        if target.value != ABSENT:
            keep.append(target.local())
        return _complement(keep)

    return select


def _untyped(ctx: _Context, checkout: _Checkout, path: str) -> bool:
    """Return whether this checkout's conflict typing for `path` is not modeled.

    The model types conflicts for files whose type every revision keeps,
    not for type changes, for a path the checkout creates together with
    its parent directory, or for a directory the run moves away. A checkout whose count could include such a path is
    not judged (`conflict_count_gap`). A directory every revision keeps
    and nothing moves (a listed or mkdir parent) is never a conflict.

    Below a directory the checkout removes (`_lost_ancestors`), a path
    neither revision holds is not modeled either: the walk never visits
    it, so it alone may keep the directory (DIRECTORY_NOT_EMPTY).
    """
    target = _entry(ctx.pinned, path, checkout.target)
    sources = [_entry(ctx.pinned, path, rev) for rev in ctx.candidates]
    if any(_retypes(source, target) for source in sources):
        return True
    if any(entry.value == DIR for entry in (*sources, target)) and _moved(ctx, path):
        return True
    if _created_with_parent(ctx, checkout, path):
        return True
    if any(
        ancestor in ctx.relevant_set
        and any(
            _retypes(
                _entry(ctx.pinned, ancestor, rev),
                _entry(ctx.pinned, ancestor, checkout.target),
            )
            for rev in ctx.candidates
        )
        for ancestor in _ancestors(path)
    ):
        return True
    return any(
        source.value == ABSENT
        and any(
            ctx.pinned.committed(a, rev) == DIR
            for a in _lost_ancestors(ctx, checkout, path)
        )
        for rev, source in zip(ctx.candidates, sources)
    )


def _lost_ancestors(ctx: _Context, checkout: _Checkout, path: str) -> list[str]:
    """Return the paths above `path` the destination holds as no directory: a
    source holding one as a directory has the checkout remove or retype it."""
    return [
        ancestor
        for ancestor in _ancestors(path)
        if ctx.pinned.committed(ancestor, checkout.target) != DIR
    ]


def _nonempty_markers(
    ctx: _Context, checkout: _Checkout, path: str
) -> dict[str, Effect]:
    """Return the DIRECTORY_NOT_EMPTY markers `path` sets, by directory.

    NORMAL keeps a conflicting path that is present, so every directory
    above it that the destination removes or turns into a non-directory
    stays, and EdenFS reports each such directory once, at every nesting
    level (`TreeInode::finalizeDirectoryRemoval`: `tryRemoveChild` fails
    on a non-empty directory). The marker is the directory's `conflict`
    cell, set in the path's apply group when the path is present and
    differs from a source the directory is removed from (the destination
    lacks the path). FORCE removes every conflicting path, so it keeps none.
    `path` carries a conflict marker (`_conflict_marker`).
    """
    if checkout.force:
        return {}
    markers: dict[str, Effect] = {}
    for directory in _lost_ancestors(ctx, checkout, path):

        def select(
            revision: int, source: _Entry, _target: _Entry, directory: str = directory
        ) -> list[_Local]:
            if ctx.pinned.committed(directory, revision) != DIR:
                return []
            return _complement([source.local(), _ABSENT_LOCAL])

        built = _by_source(ctx, checkout, path, select)
        if built is not None:
            markers[directory] = _produce(
                conflict_cell(checkout.key, directory),
                YES,
                checkout.action.id,
                Guard(*built),
            )
    return markers


def _tracks_parent(ctx: _Context, path: str) -> bool:
    """Return whether `path` always equals its entry at the EdenFS parent.

    It starts clean, no action names it or a directory above it, and
    every checkout of the run either applied in full or changed nothing:
    each one moved it with the parent, so no checkout finds it in a
    conflict state.
    """
    pinned = ctx.pinned
    start = pinned.initial_revision
    if pinned.initial(path_cell(path)) != pinned.committed(path, start):
        return False
    if pinned.initial(mode_cell(path)) != pinned.committed_mode(path, start):
        return False
    named = ctx.named
    if path in named or any(ancestor in named for ancestor in _ancestors(path)):
        return False
    return all(
        resolution(other, ctx.facts) != Resolution.INDETERMINATE
        for other in ctx.facts.actions_of("checkout")
    )


def _conflict_marker(ctx: _Context, checkout: _Checkout, path: str) -> Effect | None:
    """Return the effect marking `path` conflicting, guarded on its pre-visit state.

    The marker sits in the path's apply group, so the engine orders every
    write, chmod, or unlink of the path against the visit: one landing
    before it is in the state the guard reads (counted, then kept by
    NORMAL or reset by FORCE), one landing after it is not.
    A superseded op lands after the visit (`_supersede_groups`).
    """
    if (
        not checkout.counting
        or _untyped(ctx, checkout, path)
        or _tracks_parent(ctx, path)
    ):
        return None
    built = _by_source(
        ctx, checkout, path, _local_select(_conflict_states(checkout.force))
    )
    if built is None:
        return None
    action = checkout.action.id
    return _produce(conflict_cell(checkout.key, path), YES, action, Guard(*built))


def _recorded_conflicts(facts: ActionFacts) -> int | None:
    count = _field(facts, "conflicts")
    return count if type(count) is int else None


def _open_domain(ctx: _Context, action_id: str) -> str | None:
    """Return why paths outside the contract set may conflict, or None.

    Without class representatives an untouched path tracks the parent
    only while it starts clean and every other checkout either applied
    in full (acked) or changed nothing; otherwise its pre-visit state is
    unknown to the contracts.
    """
    if _dirt_outside(ctx):
        return "starting dirt lies on paths no observation or effect touches"
    if ctx.weights:
        return None
    for other in ctx.facts.actions_of("checkout"):
        if (
            other.id != action_id
            and resolution(other, ctx.facts) == Resolution.INDETERMINATE
        ):
            return (
                f"{other.id} may have applied in part, so an untouched path may "
                "differ from its parent"
            )
    return None


def _counting(ctx: _Context, action: Action, status: Status) -> bool:
    if (
        status != Status.ACKED
        or action.payload["via"] != "thrift"
        or ctx.obligation(CONFLICT_COUNT_RULE, action.id) is None
    ):
        return False
    recorded = ctx.facts.action(action.id)
    return (
        _recorded_conflicts(recorded) is not None
        and _open_domain(ctx, action.id) is None
    )


@dataclass(frozen=True)
class _CountPlan:
    """One counted checkout's marker paths, their `(low, high)` weights, its
    recorded count, and the relevant paths whose conflict typing is not
    modeled (`_untyped`).

    A path weighs its class size. A class representative below removed
    directories marks its own (`_nonempty_markers`), and each other member
    may add one DIRECTORY_NOT_EMPTY per level, or none when it shares a
    directory already counted: members share its presence at every
    revision, so each lies below as many removed directories, and their
    directories are not enumerated."""

    paths: tuple[str, ...]
    weights: tuple[tuple[int, int], ...]
    count: int
    untyped: tuple[str, ...]


def _count_plan(ctx: _Context, action: Action) -> _CountPlan:
    recorded = ctx.facts.action(action.id)
    assert recorded.invoked is not None and recorded.completed is not None
    count = _recorded_conflicts(recorded)
    assert count is not None
    checkout = _checkout_spec(
        ctx,
        action,
        Status.ACKED,
        Window(recorded.invoked, recorded.completed),
        counting=True,
    )
    paths, weights, untyped = [], [], []
    directories: set[str] = set()
    for path in ctx.relevant:
        if _untyped(ctx, checkout, path):
            untyped.append(path)
        elif _conflict_marker(ctx, checkout, path) is not None:
            nonempty = _nonempty_markers(ctx, checkout, path)
            weight = ctx.weights.get(path, 1)
            paths.append(path)
            weights.append((weight, weight + (weight - 1) * len(nonempty)))
            directories.update(nonempty)
    paths += sorted(directories)
    weights += [(1, 1)] * len(directories)
    return _CountPlan(tuple(paths), tuple(weights), count, tuple(untyped))


def conflict_count_gap(ctx: _Context, action: Action) -> str | None:
    """Return why a Thrift checkout's conflict count is not derived, or None when it is."""
    if _recorded_conflicts(ctx.facts.action(action.id)) is None:
        return "the checkout recorded no conflict count"
    gap = _open_domain(ctx, action.id)
    if gap is not None:
        return gap
    plan = _count_plan(ctx, action)
    if plan.untyped:
        return (
            f"conflict typing is not modeled for {plan.untyped[0]} (a type change, "
            "a path created with its parent directory, a directory the run moves, "
            "or an untracked path below a directory the checkout removes)"
        )
    return None


@dataclass(frozen=True)
class CountJudgment:
    """What the contracts says about one Thrift checkout's conflict count.

    `gap` names why it is not judged. Otherwise `markers` is how many
    paths carry a conflict marker; with none, no legal state reports a
    conflict, so the count must be 0 (and there is no engine input).
    """

    gap: str | None
    markers: int = 0


def count_judgment(
    spec: RunSpec,
    facts: HistoryFacts,
    pinned: PinnedFacts | None,
    action_id: str,
) -> CountJudgment:
    """Return how the contracts judges a Thrift checkout's conflict count."""
    ctx = _context(
        spec, facts, _eden_pinned(spec, pinned), frozenset({CONFLICT_COUNT_RULE})
    )
    action = spec.action(action_id)
    gap = conflict_count_gap(ctx, action)
    if gap is not None:
        return CountJudgment(gap)
    return CountJudgment(None, len(_count_plan(ctx, action).paths))


def count_inputs(
    ctx: _Context, action: Action, groups: tuple[EffectGroup, ...]
) -> tuple[Observation | Before, ...]:
    """Return a counted checkout's conflict-count observation and its orderings.

    `groups` are every contract group of the run. The observation reads
    every marker at the checkout's completion, after all of its per-path
    groups (one `Before` each, since the count is reported only once the
    walk ends), and counts the set markers (`Count`: class representatives
    weigh their class size, or a range, `_CountPlan`) to the recorded count.
    A checkout with no marker has no input (`count_judgment`).
    """
    recorded = ctx.facts.action(action.id)
    ref = ctx.obligation(CONFLICT_COUNT_RULE, action.id)
    if (
        ref is None
        or action.payload["via"] != "thrift"
        or resolution(recorded) != Resolution.ACKED
        or conflict_count_gap(ctx, action) is not None
    ):
        return ()
    assert recorded.completed is not None
    plan = _count_plan(ctx, action)
    if not plan.paths:
        return ()
    key = ctx.checkout_keys[action.id]
    cells = tuple(conflict_cell(key, path) for path in plan.paths)
    observation = Observation(
        id=f"o:{action.id}:conflicts",
        action=action.id,
        cells=cells,
        expect=Expect(count=Count(YES, plan.weights, plan.count)),
        window=Window(recorded.completed, recorded.completed),
        channel="thrift",
        obligations=(ref,),
    )
    marked = set(cells)
    orderings = tuple(
        Before(group.id, observation.id, ref)
        for group in groups
        if group.action == action.id
        and any(effect.cell in marked for effect in group.effects)
    )
    return (observation, *orderings)


def _rejected_groups(ctx: _Context, action: Action) -> tuple[EffectGroup, ...]:
    """Return a rejected checkout's would-be groups, each requiring the impossible.

    A checkout the guard rejected has no effects. Its
    would-be groups are derived as indeterminate inside its own interval,
    and each requires a state no value has, under the attempt's
    `checkout.in-progress-rejection` obligation: an explanation may use
    them only by violating that obligation, so an observation only they
    explain forms one proof with it. Their other
    requirements are dropped, so they implicate no other rule.
    """
    recorded = ctx.facts.action(action.id)
    ref = ctx.obligation(IN_PROGRESS_RULE, action.id)
    if (
        ref is None
        or not is_in_progress_rejection(recorded)
        or recorded.invoked is None
        or recorded.completed is None
    ):
        return ()
    never = (Requirement((parent_cell(),), _single(NEVER), ref),)
    window = Window(recorded.invoked, recorded.completed)
    return tuple(
        replace(group, requires=never)
        for group in _checkout_groups(ctx, action, Status.INDETERMINATE, window)
    )


def _interruption_groups(ctx: _Context, action: Action) -> tuple[EffectGroup, ...]:
    """Return the group by which a checkout may leave EdenFS interrupted, naming it.

    A checkout that may have failed after it got past the guard
    (`_may_interrupt`) without EdenFS reporting it (a transport error, a
    client timeout while the daemon kept running, a killed `sl`) may
    have left the mount InterruptedCheckout or completed, so it gets one
    optional group inside its effect window that sets the `interrupted`
    cell to its key. A reported failure names itself in its parent group
    (`_parent_group`).
    """
    recorded = ctx.facts.action(action.id)
    if not _may_interrupt(recorded, ctx.facts) or _reported_failure(recorded):
        return ()
    status = Status.INDETERMINATE
    window = effect_window(recorded, Resolution.INDETERMINATE, ctx.boundary)
    value = interrupted_value(ctx.checkout_keys[action.id])
    return (
        _group(
            action,
            status,
            window,
            (_produce(interrupted_cell(), value, action.id),),
            part="interrupted",
        ),
    )


def state_observation(
    ctx: _Context, action: Action, schema: ResultSchema
) -> Observation | None:
    """Return the observation a state-dependent failure makes, for `core.input-legality`.

    A failure whose errno the kind declares STATE (or an errno-less read
    failure, which `NO_ERRNO` declares) says the cells it names held one
    of the classes of value its meaning lists at some point inside the
    call; a failure meaning `_FORBIDDEN` says no value could. Its
    obligation is the action's `core.input-legality` one.

    A `getattr` failing ENOENT is the existence property's absence
    observation (`existence.missing-after-create`, declared for every
    absence); while that rule is registered the `core.input-legality`
    observation holds in every state, so the evidence files under one rule.
    """
    recorded = ctx.facts.action(action.id)
    if recorded.outcome != Outcome.FAIL or recorded.result is None:
        return None
    if _fault_caused(ctx.facts, recorded, schema):
        return None
    if action.kind == "status":
        return _status_observation(ctx, action, recorded, schema)
    if action.kind in ("sl_rm", "sl_mv"):
        return process_failure_observation(ctx, action)
    declared = NO_ERRNO if _errno_of(recorded) is None else _errno_name(recorded)
    if schema.errnos.get(declared) != ErrnoUse.STATE:
        return None
    ref = ctx.obligation(INPUT_LEGALITY_RULE, action.id)
    name = failure_errno_name(recorded)
    if ref is not None and (action.kind, name) in _UNOPENED:
        return _unopened_observation(ctx, action, recorded, ref)
    if (action.kind, name) in NONEMPTY_DESTINATION_ERRNOS:
        rows = nonempty_destination_rows(ctx, action.payload)
    else:
        meaning = None if name is None else _STATE_MEANINGS.get((action.kind, name))
        rows = None if meaning is None else meaning(action.payload)
    if name is None or rows is None or ref is None or recorded.completed is None:
        return None
    assert recorded.invoked is not None
    named = action.payload.get("path", action.payload.get("source"))
    # Under the mount root no state allows a missing parent or a file above,
    # and no state lets a rename onto an empty directory fail.
    rows = rows or (((PATH, named, NEVER),),)
    cells, expect = _meaning_rows(rows)
    if (action.kind, name) == (
        "getattr",
        "ENOENT",
    ) and GETATTR_ABSENCE_RULE in ctx.rules:
        # The existence property files this absence; one rule per evidence.
        expect = Expect((tuple(ANY for _ in cells),))
    return Observation(
        id=f"o:{action.id}:{name.lower()}",
        action=action.id,
        cells=cells,
        expect=expect,
        window=Window(recorded.invoked, recorded.completed),
        channel=action.payload.get("channel", "fuse"),
        obligations=(ref,),
    )


_GUARD_TYPES = frozenset({CHECKOUT_IN_PROGRESS})


def _refusal_text(recorded: ActionFacts) -> str | None:
    """Return which guard message a refusal carries, or None when its result carries none.

    `sl` prints it on its first stderr line; Thrift records the EdenError's
    message in `reason`.
    """
    for carrier in (_stderr_first_line(recorded), _field(recorded, "reason")):
        if not isinstance(carrier, str):
            continue
        for text in (_INTERRUPTED_TEXT, _IN_PROGRESS_TEXT, _STATUS_IN_PROGRESS_TEXT):
            if text in carrier:
                return text
    return None


def _guard_expect(
    ctx: _Context, recorded: ActionFacts, interrupted: Atom | None
) -> Expect:
    """Return what a refusal by EdenFS's checkout guard says, over the `interrupted` cell.

    EdenFS words the refusal by its state (`EdenMount.cpp:1757-1783`,
    `co_diff`): the interrupted advice while an interruption stands, which
    `interrupted` (an atom over the cell, None when no checkout can
    explain it) describes; the in-progress text while another checkout
    holds the guard, which is not a cell (`guard_holders`), so that row
    holds in every state or in none. A refusal whose message the result
    does not carry may be either.
    """
    text = _refusal_text(recorded)
    rows: list[tuple[Atom, ...]] = []
    if text != _INTERRUPTED_TEXT and guard_holders(ctx.facts, recorded):
        rows.append((ANY,))
    in_progress = text in (_IN_PROGRESS_TEXT, _STATUS_IN_PROGRESS_TEXT)
    if not in_progress and interrupted is not None:
        rows.append((interrupted,))
    return Expect(tuple(rows) or ((NEVER,),))


def served_observation(ctx: _Context, action: Action) -> Observation | None:
    """Return the observation that a served status ran while no checkout was
    interrupted (`EdenMount::co_diff` refuses, EdenMount.cpp:2509-2527)."""
    recorded = ctx.facts.action(action.id)
    ref = ctx.obligation(INTERRUPTED_REFUSAL_RULE, action.id)
    if (
        ref is None
        or not ctx.interruptible
        or action.kind != "status"
        or recorded.outcome != Outcome.OK
        or recorded.invoked is None
        or recorded.completed is None
    ):
        return None
    return Observation(
        id=f"o:{action.id}:served",
        action=action.id,
        cells=(interrupted_cell(),),
        expect=_single(eq(ABSENT)),
        window=Window(recorded.invoked, recorded.completed),
        channel=action.payload.get("channel", "sl"),
        obligations=(ref,),
    )


def rejection_observation(ctx: _Context, action: Action) -> Observation | None:
    """Return what a checkout the guard refused says, for `checkout.in-progress-rejection`.

    With the interrupted advice EdenFS refuses only a target other than
    the interrupted checkout's (a same target resumes),
    so inside the call the `interrupted` cell names a checkout to another
    target; with the in-progress text another checkout holds the guard
    (`_guard_expect`).
    """
    recorded = ctx.facts.action(action.id)
    ref = ctx.obligation(IN_PROGRESS_RULE, action.id)
    if ref is None or not is_in_progress_rejection(recorded):
        return None
    if recorded.completed is None:
        return None
    assert recorded.invoked is not None
    target = action.payload["revision"]
    others = frozenset(
        interrupted_value(ctx.checkout_keys[other.id])
        for other in ctx.facts.actions_of("checkout")
        if other.payload["revision"] != target and _may_interrupt(other, ctx.facts)
    )
    return Observation(
        id=f"o:{action.id}:refused",
        action=action.id,
        cells=(interrupted_cell(),),
        expect=_guard_expect(ctx, recorded, one_of(others) if others else None),
        window=Window(recorded.invoked, recorded.completed),
        channel="thrift" if action.payload["via"] == "thrift" else "sl",
        obligations=(ref,),
    )


def _guard_refusal(recorded: ActionFacts) -> bool | None:
    """Classify a failed status: True for a checkout-guard refusal, False for
    any other typed failure, None when it carries no type.

    Thrift names `CHECKOUT_IN_PROGRESS` (`EdenMount::co_diff` refuses while
    a checkout runs or stays interrupted). An `sl status` that exits
    nonzero reports the same refusal on its first stderr line once
    Sapling's 60 s poll gives up (`abort: EdenError: cannot compute status
    while a checkout is currently in progress`), or the interrupted-checkout advice. The text a second
    checkout is rejected with never comes from a status. An `sl status`
    failure without an exit status (its `sl diff` step failed) carries no
    type.
    """
    error_type = _field(recorded, "error_type")
    if isinstance(error_type, str):
        return error_type in _GUARD_TYPES
    if _field(recorded, "returncode") is None:
        return None
    line = _stderr_first_line(recorded)
    return _STATUS_IN_PROGRESS_TEXT in line or _INTERRUPTED_TEXT in line


def _status_observation(
    ctx: _Context, action: Action, recorded: ActionFacts, schema: ResultSchema
) -> Observation | None:
    """Return what a failed status says about state, for `core.input-legality`.

    - Thrift `OUT_OF_DATE_PARENT`: the requested commit is not EdenFS's
      parent (`_out_of_date_rows`).
    - A checkout-guard refusal (Thrift `CHECKOUT_IN_PROGRESS`, `sl status`
      exiting with the in-progress or interrupted text): with the
      interrupted advice some checkout is interrupted inside the call
      (the `interrupted` cell is set); with the in-progress text another
      checkout held or may have held the guard (`_guard_expect`).
    - Any other exit of `sl status` (no state allows it):
      no state. A SIGKILL exit says nothing about state: no observation,
      so the core excuses it when a kill nemesis targeted the status and
      leaves it unobserved otherwise.
    - Untyped (`sl diff` failed): unobserved, so not evaluated.

    An undeclared Thrift error type is judged by the core directly.
    """
    ref = ctx.obligation(INPUT_LEGALITY_RULE, action.id)
    error_type = _field(recorded, "error_type")
    declared = (
        schema.error_types.get(error_type)
        if isinstance(error_type, str)
        else schema.errnos.get(NO_ERRNO)
    )
    if (
        ref is None
        or recorded.completed is None
        or declared != ErrnoUse.STATE
        or _field(recorded, "returncode") == KILLED
    ):
        return None
    assert recorded.invoked is not None
    if error_type == _OUT_OF_DATE_PARENT:
        cells, expect = _out_of_date_rows(ctx, action)
        name = "out-of-date-parent"
    else:
        refusal = _guard_refusal(recorded)
        if refusal is None:
            return None
        if refusal:
            cells = (interrupted_cell(),)
            interrupted = none_of({ABSENT}) if ctx.interruptible else None
            expect = _guard_expect(ctx, recorded, interrupted)
        else:
            cells, expect = (parent_cell(),), _single(NEVER)
        name = "guard" if refusal else "exit"
    return Observation(
        id=f"o:{action.id}:{name}",
        action=action.id,
        cells=cells,
        expect=expect,
        window=Window(recorded.invoked, recorded.completed),
        channel=action.payload.get("channel", "sl"),
        obligations=(ref,),
    )


def _source_choices(
    ctx: _Context, action: Action
) -> tuple[tuple[Cell, ...], list[tuple[int, tuple[Atom, ...]]]]:
    """Return the cells naming an attempt's source and, per source revision, their rows.

    The source is the EdenFS parent, or, while the `interrupted` cell
    names a checkout to the same destination the attempt may resume, that
    checkout's source (the resume diffs against it; only an
    interrupted checkout is resumed, `EdenMount.cpp:1757-1783`), read from
    its marks or its settled source. While the cell names one, the parent
    already holds the destination and is not the source.
    """
    plan = ctx.plans.get(action.id, _Plan(None, True, ()))
    parent = parent_cell()
    if not plan.resumes:
        return (parent,), [(r, (eq(rev_value(r)),)) for r in ctx.candidates]
    marked = [resume for resume in plan.resumes if resume.source is None]
    target = action.payload["revision"]
    destination = eq(rev_value(target))
    resuming = none_of({interrupted_value(resume.key) for resume in plan.resumes})
    cells = (
        parent,
        interrupted_cell(),
        *(
            source_cell(resume.key, revision)
            for resume in marked
            for revision in ctx.candidates
        ),
    )
    free = tuple(ANY for _ in cells[2:])
    choices: list[tuple[int, tuple[Atom, ...]]] = []
    for revision in ctx.candidates:
        plain = resuming if revision == target else ANY
        choices.append((revision, (eq(rev_value(revision)), plain, *free)))
        for index, resume in enumerate(marked):
            column = index * len(ctx.candidates) + ctx.candidates.index(revision)
            names = eq(interrupted_value(resume.key))
            marks = tuple(eq(YES) if j == column else ANY for j in range(len(free)))
            choices.append((revision, (destination, names, *marks)))
        for resume in plan.resumes:
            if resume.source == revision:
                names = eq(interrupted_value(resume.key))
                choices.append((revision, (destination, names, *free)))
    return cells, choices


def abort_scope(action_id: str, part: str) -> str:
    """Return the `checkout.sl-abort` scope of one input class of an aborting update.

    It is `<action>:<part>`, `part` `ABORT_LISTED`; every other input of
    the rule is scoped by the action alone. Each scope is its own
    obligation, so the engine decides, and the identity names, which class
    of input the abort contradicts. `part` must name an `ABORT_BLOCKERS` entry.
    """
    if not part or part not in ABORT_BLOCKERS:
        raise ValueError(f"checkout.sl-abort scope part {part!r} has no blockers")
    return f"{action_id}:{part}"


def abort_observations(ctx: _Context, action: Action) -> tuple[Observation, ...]:
    """Return what an aborting `sl goto` says: each path it listed blocked.

    The DRY_RUN walk reads paths one at a time, not as a snapshot, so each
    listed path gets its own observation inside the call, conjoined with
    the cells naming the source manifest (`_source_choices`; nothing
    changes on abort): it blocked, so an abort has a blocker even when
    Sapling truncated the list. A listed path the
    destination leaves unchanged from every candidate source cannot be
    listed, so no row explains it. The observations support the
    `<action>:listed` obligation (`abort_scope`).
    """
    recorded = ctx.facts.action(action.id)
    paths = tuple(dict.fromkeys(conflict_paths(recorded)))
    listed = ctx.obligation(CHECKOUT_ABORT_RULE, abort_scope(action.id, ABORT_LISTED))
    if not paths or listed is None or recorded.completed is None:
        return ()
    assert recorded.invoked is not None
    window = Window(recorded.invoked, recorded.completed)
    target = action.payload["revision"]
    source_cells, choices = _source_choices(ctx, action)
    observations = []
    for path in paths:
        rows = []
        destination = _entry(ctx.pinned, path, target)
        for revision, prefix in choices:
            passes = _sl_passes(_entry(ctx.pinned, path, revision), destination)
            if passes is None:
                continue
            rows += [(*prefix, *local) for local in _complement(passes)]
        observations.append(
            Observation(
                id=f"o:{action.id}:abort:{_path_part(path)}",
                action=action.id,
                cells=(*source_cells, *_local_cells(path)),
                expect=Expect(
                    tuple(dict.fromkeys(rows))
                    or ((NEVER, *(ANY for _ in source_cells[1:]), *_ANY_LOCAL),)
                ),
                window=window,
                channel="sl",
                obligations=(listed,),
            )
        )
    return tuple(observations)


@dataclass(frozen=True)
class DescriptorPlan:
    """One held descriptor the contracts tracks: its open, key, path, reachable paths, gaps.

    `paths` holds the open's path and every path a rename or `sl mv` of
    the run can move its object through, before or after the open (a
    closure over the moved pairs, both ways). `gaps` are the overlaps
    inside which the object is not modeled (`gap`).
    """

    open_id: str
    key: str
    path: str
    paths: frozenset[str]
    opened: Window
    gaps: tuple[DescriptorGap, ...] = ()

    def gap(self, read: Window) -> str | None:
        """Return why a read inside `read` is not modeled, or None.

        Only an overlap that can fall between the open's invocation and
        the read's completion can change what the read returns.
        """
        for found in self.gaps:
            if found.window.lo <= read.hi and self.opened.lo <= found.window.hi:
                return found.reason
        return None


def _superseding_gaps(
    ctx: _Context, paths: frozenset[str]
) -> tuple[DescriptorGap, ...]:
    """Return every overlap of a write through `paths` with a checkout changing that path."""
    gaps: list[DescriptorGap] = []
    for checkout in ctx.facts.actions_of("checkout"):
        status = resolution(checkout, ctx.facts)
        if status == Resolution.NONE:
            continue
        window = effect_window(checkout, status, ctx.boundary)
        target = checkout.payload["revision"]
        for path in sorted(paths):
            destination = _entry(ctx.pinned, path, target)
            if not any(
                _changes(_entry(ctx.pinned, path, revision), destination)
                for revision in ctx.candidates
            ):
                continue
            for write in ctx.ops.get(path, ()):
                if write.cell.space != PATH or not write.window.meets(window):
                    continue
                gaps.append(
                    DescriptorGap(
                        Window(
                            max(write.window.lo, window.lo),
                            min(write.window.hi, window.hi),
                        ),
                        f"write {write.action} to {path} overlaps checkout "
                        f"{checkout.id}: whether it lands in the replaced inode the "
                        "descriptor holds is not modeled",
                    )
                )
    return tuple(gaps)


def _descriptor_plans(ctx: _Context) -> dict[str, DescriptorPlan]:
    """Return a plan per acked open some `read_fd` reads through."""
    read = {
        action.payload["fd"] for action in ctx.spec.actions if action.kind == "read_fd"
    }
    pairs = _move_pairs(ctx)
    keys = descriptor_keys(ctx.facts)
    plans: dict[str, DescriptorPlan] = {}
    for action in ctx.spec.actions:
        if action.kind != "open" or action.id not in read:
            continue
        recorded = ctx.facts.action(action.id)
        if resolution(recorded) != Resolution.ACKED:
            continue
        path = normalize_path(action.payload["path"])
        paths = _reachable(path, pairs)
        plans[action.id] = DescriptorPlan(
            action.id,
            keys[action.id],
            path,
            paths,
            effect_window(recorded, Resolution.ACKED, ctx.boundary),
            _superseding_gaps(ctx, paths),
        )
    return plans


def descriptor_plans(
    spec: RunSpec, facts: HistoryFacts, pinned: PinnedFacts | None
) -> Mapping[str, DescriptorPlan]:
    """Return the held descriptors the contracts tracks, by open ID (`_descriptor_plans`)."""
    ctx = _context(spec, facts, _eden_pinned(spec, pinned), CONTRACT_RULES)
    return _descriptor_plans(ctx)


def _object_effects(
    plan: DescriptorPlan, action: Action, effect: Effect
) -> tuple[list[Effect], list[Effect]]:
    """Return what one effect on a path the object can reach does to the descriptor.

    The first list joins the effect's group. The second holds unbinds that
    may or may not happen with it: a checkout that leaves the path's value
    as it was may skip the entry or replace it (EdenFS short-circuits a
    clean subtree already at the destination, `TreeInode.cpp`
    `canShortCircuitCheckout`, even under FORCE, and replaces a loaded
    inode its walk reaches), so only a change of value surely unbinds.
    """
    opened = descriptor_cell(plan.key)
    unopened = ((opened,), [(eq(ABSENT),)])
    if effect.kind == EffectKind.TRANSFER:
        assert effect.source is not None
        source, destination = effect.source.key, effect.cell.key
        mirror = Effect(
            object_cell(plan.key, destination),
            EffectKind.TRANSFER,
            None,
            object_cell(plan.key, source),
            action.id,
            _conjoined(*unopened, None),
        )
        binding = _transfer(
            binding_cell(plan.key, destination),
            binding_cell(plan.key, source),
            action.id,
        )
        return [mirror, binding], []
    path = effect.cell.key
    assert effect.value is not None
    bound = ((binding_cell(plan.key, path),), [(eq(BOUND),)])
    if action.kind == "write" and path == plan.path:
        either = (
            (opened, binding_cell(plan.key, path)),
            [(eq(ABSENT), ANY), (ANY, eq(BOUND))],
        )
        return [
            _produce(
                object_cell(plan.key, path),
                effect.value,
                action.id,
                _conjoined(*either, effect.guard),
            )
        ], []
    mirror = _produce(
        object_cell(plan.key, path),
        effect.value,
        action.id,
        _conjoined(*unopened, effect.guard),
    )
    if action.kind == "write":
        held = _produce(
            object_cell(plan.key, plan.path),
            effect.value,
            action.id,
            _conjoined(*bound, effect.guard),
        )
        return [mirror, held], []
    if action.kind != "checkout":
        unbind = _produce(
            binding_cell(plan.key, path),
            ABSENT,
            action.id,
            _conjoined(*bound, effect.guard),
        )
        return [mirror, unbind], []
    changed, same = _split_on_value(effect.guard, effect.cell, effect.value)
    surely = [
        _produce(
            binding_cell(plan.key, path),
            ABSENT,
            action.id,
            _conjoined(*bound, guard),
        )
        for guard in (changed,)
        if guard is not None
    ]
    maybe = [
        _produce(
            binding_cell(plan.key, path),
            ABSENT,
            action.id,
            _conjoined(*bound, guard),
        )
        for guard in (same,)
        if guard is not None
    ]
    return [mirror, *surely], maybe


def _with_descriptors(
    plans: Iterable[DescriptorPlan], action: Action, group: EffectGroup
) -> list[EffectGroup]:
    """Return `group` plus its effects on every tracked descriptor's cells.

    Unbinds that may or may not happen with the group come back as their
    own indeterminate groups inside its window, one per descriptor and path,
    carrying the group's requirements: a rejected checkout's unbind needs
    the same counterfactual as the rest of it.
    """
    extra: list[Effect] = []
    optional: list[EffectGroup] = []
    for plan in plans:
        for effect in group.effects:
            if effect.cell.space != PATH:
                continue
            touched = {effect.cell.key}
            if effect.source is not None:
                touched.add(effect.source.key)
            if not touched & plan.paths:
                continue
            joined, maybe = _object_effects(plan, action, effect)
            extra += joined
            for unbind in maybe:
                optional.append(
                    EffectGroup(
                        id=f"{group.id}:{plan.key}:{_path_part(effect.cell.key)}",
                        action=group.action,
                        status=Status.INDETERMINATE,
                        window=group.window,
                        effects=(unbind,),
                        requires=group.requires,
                    )
                )
    found = group if not extra else replace(group, effects=(*group.effects, *extra))
    return [found, *optional]


def _directory_object_observation(
    ctx: _Context, plans: Mapping[str, DescriptorPlan], action: Action
) -> Observation | None:
    """Return what a `read_fd` failing EISDIR says: its descriptor's object is a directory.

    `open(O_RDONLY)` of a directory succeeds on FUSE (OPENDIR) and `read`
    on that descriptor fails EISDIR. The object the open bound is
    `object_cell` of its path, so that cell holds a directory inside the
    read's call. Without a tracked descriptor (an open not acked) the
    obligation stays not evaluated.
    """
    recorded = ctx.facts.action(action.id)
    ref = ctx.obligation(INPUT_LEGALITY_RULE, action.id)
    plan = plans.get(action.payload["fd"])
    if (
        ref is None
        or plan is None
        or recorded.outcome != Outcome.FAIL
        or recorded.completed is None
        or _errno_of(recorded) != errno.EISDIR
    ):
        return None
    assert recorded.invoked is not None
    return Observation(
        id=f"o:{action.id}:eisdir",
        action=action.id,
        cells=(object_cell(plan.key, plan.path),),
        expect=_single(eq(DIR)),
        window=Window(recorded.invoked, recorded.completed),
        channel="fuse",
        obligations=(ref,),
    )


def _open_group(ctx: _Context, plan: DescriptorPlan) -> EffectGroup:
    """The open marks its descriptor and binds its path's object."""
    action = ctx.spec.action(plan.open_id)
    return _group(
        action,
        Status.ACKED,
        plan.opened,
        (
            _produce(descriptor_cell(plan.key), OPENED, action.id),
            _produce(binding_cell(plan.key, plan.path), BOUND, action.id),
        ),
    )


def _descriptor_groups(
    ctx: _Context,
    plans: Mapping[str, DescriptorPlan],
    action: Action,
    groups: tuple[EffectGroup, ...],
) -> tuple[EffectGroup, ...]:
    if not plans:
        return groups
    found = tuple(
        item
        for group in groups
        for item in _with_descriptors(plans.values(), action, group)
    )
    plan = plans.get(action.id)
    if action.kind == "open" and plan is not None:
        found = (*found, _open_group(ctx, plan))
    return found


def _window_of(facts: ActionFacts, boundary: float) -> Window | None:
    """Return the interval a checkout held or may have held EdenFS's guard.

    A typed Thrift rejection never held it. `sl goto` takes it twice,
    for DRY_RUN and then NORMAL (`edenfs.rs:378-410`), so a refused `sl`
    may have held it until its completion. An attempt that completed with
    resolved effects held it at most until its completion, and so did a
    failure EdenFS itself reported (an `EdenError`: Thrift records its
    type, `sl` prints `abort: EdenError:`), since the daemon replies only
    once its checkout ended; any other indeterminate one (a transport
    error, a client timeout, a killed `sl`), even one with a completion
    time, until the resolution boundary.
    """
    if facts.invoked is None or (
        is_in_progress_rejection(facts) and facts.payload.get("via") == "thrift"
    ):
        return None
    status = resolution(facts)
    reported = _reported_failure(facts)
    if facts.completed is None or (status == Resolution.INDETERMINATE and not reported):
        return effect_window(facts, Resolution.INDETERMINATE, boundary)
    return Window(facts.invoked, facts.completed)


def _reported_failure(facts: ActionFacts) -> bool:
    """Return whether EdenFS itself reported a checkout's failure, other than a guard refusal.

    Thrift records the EdenError's type; `sl` prints `abort: EdenError:`.
    The daemon replies only once its checkout ended. A refused `set_parents`
    after a completed walk is not the checkout's failure.
    """
    if (
        facts.kind != "checkout"
        or facts.outcome != Outcome.FAIL
        or is_in_progress_rejection(facts)
    ):
        return False
    first = _stderr_first_line(facts)
    if first.startswith(_SL_SET_PARENTS_REFUSAL):
        return False
    return isinstance(_field(facts, "error_type"), str) or first.startswith(
        "abort: EdenError:"
    )


def _may_interrupt(facts: ActionFacts, history: HistoryFacts | None = None) -> bool:
    """Return whether a checkout may have reached start and then failed.

    Any checkout whose effects are indeterminate may have: a failure
    mid-walk, a transport error, a killed `sl`, or a client timeout while
    the daemon kept running.
    """
    return (
        facts.kind == "checkout"
        and resolution(facts, history) == Resolution.INDETERMINATE
    )


def guard_holders(facts: HistoryFacts, action: ActionFacts) -> tuple[str, ...]:
    """Return the checkouts that held, or may have held, EdenFS's checkout guard while `action` ran.

    Holding the guard spans a whole checkout, so it is not a cell: it is
    read from the recorded windows (`_window_of`). `action` must have
    completed.
    """
    assert action.invoked is not None and action.completed is not None
    boundary = resolution_boundary(facts)
    own = action.window
    return tuple(
        other.id
        for other in facts.actions_of("checkout")
        if other.id != action.id
        and (window := _window_of(other, boundary)) is not None
        and window.meets(own)
    )


def known_prestart(facts: HistoryFacts, checkout: ActionFacts) -> bool:
    """An exclusive, fully prearmed begin-error guarantees failure before start.

    EdenMount restores oldState before start (D129789564). Other possible
    consumers, removals or unreported outcomes leave the phase ambiguous.
    """
    if not _reported_failure(checkout):
        return False
    assert checkout.invoked is not None and checkout.completed is not None
    boundary = resolution_boundary(facts)
    for install in facts.actions_of("fault_install"):
        if (
            install.outcome != Outcome.OK
            or install.payload.get("site") != CHECKOUT_BEGIN_SITE
            or install.payload.get("behavior") != "error"
            or install.payload.get("ordinal", 1) != 1
            or install.payload.get("target_action") != checkout.id
            or install.completed is None
            or install.completed >= checkout.invoked
        ):
            continue
        assert install.invoked is not None
        armed = Window(install.invoked, checkout.completed)
        if any(
            other.id != checkout.id
            and (window := _window_of(other, boundary)) is not None
            and window.meets(armed)
            for other in facts.actions_of("checkout")
        ):
            continue
        if any(
            remove.payload.get("fault_id") == install.payload.get("fault_id")
            and remove.invoked is not None
            and install.invoked <= remove.invoked <= checkout.completed
            for remove in facts.actions_of("fault_remove")
        ):
            continue
        return True
    return False
