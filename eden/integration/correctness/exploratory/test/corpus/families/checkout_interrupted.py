# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Corpus family `checkout`, interrupted checkouts: what an interrupted mount
serves and refuses, sl goto resume, and `--continue`."""

from __future__ import annotations

from eden.integration.correctness.exploratory.test.corpus.author import (
    committed,
    NOT_EVALUATED,
    R0,
    R1,
    R2,
    read_ok,
    Run,
    SATISFIED,
    sl_abort,
    sl_checkout,
    sl_status,
    thrift_checkout,
    thrift_status,
    TIP,
    VIOLATED,
    WRITE_OK,
)
from eden.integration.correctness.exploratory.test.corpus.families.common import (
    CHECKOUT_GUARD,
    inputs_ok,
    INTERRUPTED_CHECKOUT,
    process_fail,
    SL_ABORT_OR_SUCCESS,
    sl_abort_rule,
    SL_GOTO_CONTRACT,
    stale_read,
)


def _interrupted(
    name: str, description: str, *, read_first: bool, basis: tuple[str, ...] = ()
) -> Run:
    run = Run("checkout", name, description, basis=[INTERRUPTED_CHECKOUT, *basis])
    if read_first:
        run.act("o", "r0", "read", path=R0, channel="fuse")
        run.act(
            "a",
            "c",
            "checkout",
            revision=2,
            via="thrift",
            mode="normal",
            requires=[("r0", "finished")],
        )
        run.call("r0", "ok", **read_ok(committed(R0, 2)))
        run.call(
            "c",
            "fail",
            reason="std::runtime_error: interrupted",
            source="thrift",
            error_type="GENERIC_ERROR",
        )
    else:
        run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
        run.act("a", "r0", "read", path=R0, channel="fuse")
        run.act("a", "r1", "read", path=R1, channel="fuse")
        run.call(
            "c",
            "fail",
            reason="std::runtime_error: interrupted",
            source="thrift",
            error_type="GENERIC_ERROR",
        )
        run.call("r0", "ok", **read_ok(committed(R0, 2)))
        run.call("r1", "ok", **read_ok(committed(R1, TIP)))
        inputs_ok(run, "r1")
    run.finish()
    inputs_ok(run, "r0")
    return run


def _interrupted_runs() -> list[Run]:
    run = _interrupted(
        "interrupted-checkout-independent-paths",
        "A Thrift NORMAL to rev2 "
        "fails mid-walk; r0 then reads rev2's bytes while r1 still reads "
        "rev3's: per-path progress is an optional finite choice.",
        read_first=False,
        basis=(
            "INFERRED: r0 and r1 share tree/registers; a live interrupted checkout "
            "shows independent progress across directories and, inside one directory, "
            "only all-or-nothing or all-applied outcomes. Mixed progress inside one "
            "directory needs a loaded child whose CheckoutAction fails while a "
            "sibling's succeeded (TreeInode.cpp:4660-4670). The contracts encodes "
            "per-path independence as a sound over-approximation.",
        ),
    )
    stale_read(run, "r0", R0, SATISFIED, "c's r0 effect happened")
    stale_read(run, "r1", R1, SATISFIED, "c's r1 effect did not happen")
    run2 = _interrupted(
        "interrupted-checkout-read-before-invoke",
        "A read returns rev2's "
        "r0 bytes before the interrupted checkout to rev2 is invoked.",
        read_first=True,
    )
    stale_read(
        run2, "r0", R0, VIOLATED, "c's only r0 producer starts after the read ends"
    )
    return [run, run2]


SL_INTERRUPTED_STDERR = "abort: EdenError: std::runtime_error: interrupted\n"


INTERRUPTED_ADVICE = (
    "a previous checkout was interrupted - please run 'sl go {node}' to resume it"
)


INTERRUPTED_REFUSALS = (
    "while a checkout is interrupted, EdenFS refuses checkouts to other targets "
    "and status"
)


def _known_interrupted(name: str, description: str, *, basis: list[str]) -> Run:
    """A destination-byte witness proves the reported failure got past start."""
    run = Run(
        "checkout",
        name,
        description,
        basis=[CHECKOUT_GUARD, INTERRUPTED_REFUSALS, *basis],
    )
    run.act("a", "c1", "checkout", revision=2, via="thrift", mode="normal")
    run.call(
        "c1",
        "fail",
        reason="checkout: EdenError: mid-walk failure",
        source="thrift",
        error_type="GENERIC_ERROR",
    )
    run.act("a", "started", "read", path=R0, channel="fuse")
    run.call("started", "ok", **read_ok(committed(R0, 2)))
    return run


def _start_proof(run: Run, outcome: str) -> None:
    reason = (
        "the destination-byte witness requires c1's start and standing interruption"
    )
    stale_read(run, "started", R0, outcome, reason)
    run.expect(
        "checkout.parent-before-paths",
        "c1",
        outcome,
        reason,
        identity={"via": "thrift"},
    )


def _empty_thrift_status_proof(run: Run) -> None:
    """The empty status participates in the contradiction of c1's proven start."""
    for path in (
        "tree/a0/000/00002.dat",
        "tree/a0/r1/00000.dat",
        "tree/a0/r3/00002.dat",
        "tree/a1/r2/00001.dat",
        "tree/churn/c0",
        "tree/churn/c1",
        "tree/churn/c2",
        "tree/churn/c3",
        R0,
        R1,
        R2,
    ):
        run.expect(
            "status.thrift-model-stale-status",
            "s",
            VIOLATED,
            "the empty status requires effects whose start would refuse it",
            path=path,
            identity={"observed": "absent", "producer-kinds": "checkout"},
        )


def interrupted_runs() -> list[Run]:
    runs = []
    for served in (False, True):
        run = _known_interrupted(
            (
                "interrupted-next-checkout-applied"
                if served
                else "interrupted-next-checkout-rejected"
            ),
            (
                "After a known interruption, a Thrift checkout to another revision "
                "succeeds."
                if served
                else "After a known interruption, a Thrift checkout to another "
                "revision is rejected with the interrupted advice."
            ),
            basis=["edenfs/properties/checkout.py interruption and rejection checks"],
        )
        run.act("a", "c2", "checkout", revision=1, via="thrift", mode="normal")
        if served:
            run.call("c2", "ok", **thrift_checkout(1, conflicts=0))
        else:
            run.call(
                "c2",
                "fail",
                reason=INTERRUPTED_ADVICE.format(node="corpus"),
                source="thrift",
                error_type="CHECKOUT_IN_PROGRESS",
            )
        run.finish()
        if served:
            run.expect(
                "checkout.interrupted-refusal",
                "c2",
                VIOLATED,
                "c2 targets another revision while c1's interruption stands",
                identity={"action-kind": "checkout"},
            )
            run.expect(
                "checkout.conflict-count",
                "c2",
                NOT_EVALUATED,
                "c1's interruption leaves which paths c2 found dirty underived",
                identity={"mode": "normal"},
                not_evaluated_reason="checkout.conflict-count-unmodeled",
            )
        else:
            run.expect(
                "checkout.in-progress-rejection",
                "c2",
                SATISFIED,
                "c1 may have left the mount interrupted",
            )
        _start_proof(run, VIOLATED if served else SATISFIED)
        runs.append(run)

    for served in (False, True):
        run = _known_interrupted(
            "interrupted-status-served" if served else "interrupted-status-refused",
            (
                "After a known interruption, sl status succeeds."
                if served
                else "After a known interruption, sl status is refused with the "
                "interrupted advice."
            ),
            basis=[
                "`EdenMount::co_diff` refuses status while a checkout is interrupted"
            ],
        )
        run.act("a", "s", "status", channel="sl")
        if served:
            run.call("s", "ok", **sl_status({}, []))
        else:
            run.call(
                "s",
                "fail",
                reason="status: exit 255",
                returncode=255,
                stdout="",
                stderr="abort: EdenError: "
                + INTERRUPTED_ADVICE.format(node="corpus")
                + "\n",
            )
        run.finish()
        if served:
            run.expect(
                "checkout.interrupted-refusal",
                "s",
                VIOLATED,
                "status is refused while c1's interruption stands",
                identity={"action-kind": "status"},
            )
            run.expect(
                "status.sl-fuse-stale-status",
                "s",
                VIOLATED,
                "the destination-byte witness contradicts this empty served status",
                path=R0,
                attribution="undetermined",
                identity={"observed": "absent", "producer-kinds": "checkout"},
            )
        _start_proof(run, VIOLATED if served else SATISFIED)
        runs.append(run)
    return runs


def _sl_interrupted(name: str, description: str, *, dirty: bool) -> Run:
    run = Run(
        "checkout",
        name,
        description,
        basis=[
            "an interrupted sl goto exits 255 with an EdenError; the daemon's walk "
            "may have applied paths",
            SL_ABORT_OR_SUCCESS,
        ],
    )
    if dirty:
        run.act("a", "w", "write", path=R0, value_id="w")
    run.act("a", "c", "checkout", revision=2, via="goto", mode="normal")
    run.act("a", "r", "read", path=R0, channel="fuse")
    if dirty:
        run.call("w", "ok", **WRITE_OK)
    run.call(
        "c",
        "fail",
        reason="goto: " + SL_INTERRUPTED_STDERR.strip(),
        returncode=255,
        stdout="",
        stderr=SL_INTERRUPTED_STDERR,
    )
    run.call("r", "ok", **read_ok(committed(R0, 2)))
    run.finish()
    return run


INTERRUPTED_RESUME = (
    "an interrupted checkout: other targets refused, a same-target checkout "
    "resumes it, `--continue` needs `.hg/updatestate`"
)

NOT_INTERRUPTED = {
    "reason": "go: abort: not in an interrupted goto state",
    "returncode": 255,
    "stdout": "",
    "stderr": "abort: not in an interrupted goto state\n",
}


SL_INTERRUPTED_ADVICE = (
    "abort: EdenError: a previous checkout was interrupted - please run "
    "`sl go {node}` to resume it."
)


EDEN_ERROR = {
    "reason": "goto: abort: EdenError: std::runtime_error: fault",
    "returncode": 255,
    "stdout": "",
    "stderr": "abort: EdenError: std::runtime_error: fault\n",
}


def _thrift_interrupted(run: Run) -> None:
    run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
    run.call(
        "c", "fail", reason="thrift: fault", source="thrift", error_type="GENERIC_ERROR"
    )


def _resume(name: str, description: str, *, observe: str) -> Run:
    run = Run("checkout", name, description, basis=[INTERRUPTED_RESUME])
    _thrift_interrupted(run)
    run.act("a", "d", "checkout", revision=2, via="goto", mode="normal")
    run.act("a", "r", "read", path=R0, channel="fuse")
    run.call("d", "ok", **sl_checkout(2))
    data = {"rev2": committed(R0, 2), "rev3": committed(R0, TIP)}[observe]
    run.call("r", "ok", **read_ok(data))
    run.finish()
    return run


def _continue(name: str, description: str, *, interrupted: bool, ok: bool) -> Run:
    run = Run("checkout", name, description, basis=[INTERRUPTED_RESUME])
    run.act("a", "c", "checkout", revision=2, via="goto", mode="normal")
    run.act("a", "k", "checkout", revision=2, via="goto", mode="continue")
    if interrupted:
        run.call("c", "fail", **EDEN_ERROR)
    else:
        run.call("c", "ok", **sl_checkout(2))
    if ok:
        run.call("k", "ok", **sl_checkout(2, via="goto"))
    else:
        run.call("k", "fail", **NOT_INTERRUPTED)
    run.finish()
    return run


def resume_and_continue_runs() -> list[Run]:
    runs = []
    run = _resume(
        "sl-resume-completes-interrupted",
        "A Thrift checkout to rev2 fails mid-walk; sl goto to rev2 resumes it, "
        "and r0 then reads rev2's bytes.",
        observe="rev2",
    )
    stale_read(
        run, "r", R0, SATISFIED, "the resume applied r0 from the interrupted source"
    )
    runs.append(run)

    run = _resume(
        "sl-resume-leaves-stale-path",
        "A Thrift checkout to rev2 fails mid-walk; sl goto to rev2 succeeds, "
        "yet r0 still reads rev3's bytes.",
        observe="rev3",
    )
    stale_read(
        run,
        "r",
        R0,
        VIOLATED,
        "a successful resume applies every path the interruption left",
    )
    runs.append(run)

    run = _continue(
        "sl-continue-resumes-interrupted-update",
        "sl goto to rev2 fails with an EdenError after its DRY_RUN; "
        "`sl go --continue` then succeeds.",
        interrupted=True,
        ok=True,
    )
    sl_abort_rule(run, "k", SATISFIED, "`.hg/updatestate` may name rev2", "succeeded")
    runs.append(run)

    run = _continue(
        "sl-continue-without-interruption-succeeds",
        "sl goto to rev2 succeeds, then `sl go --continue` succeeds although "
        "no `.hg/updatestate` is left.",
        interrupted=False,
        ok=True,
    )
    sl_abort_rule(
        run, "k", VIOLATED, "a successful update deletes `.hg/updatestate`", "succeeded"
    )
    runs.append(run)

    run = _continue(
        "sl-continue-refused-after-success",
        "sl goto to rev2 succeeds, then `sl go --continue` exits 255 "
        "`not in an interrupted goto state`.",
        interrupted=False,
        ok=False,
    )
    sl_abort_rule(run, "k", SATISFIED, "no `.hg/updatestate` is left", "aborted")
    runs.append(run)

    run = _continue(
        "sl-continue-refused-while-interrupted",
        "sl goto to rev2 fails after its DRY_RUN left `.hg/updatestate`; "
        "`sl go --continue` right after exits `not in an interrupted goto state`.",
        interrupted=True,
        ok=False,
    )
    sl_abort_rule(
        run,
        "k",
        SATISFIED,
        "the failed update may have failed before its DRY_RUN passed, writing nothing",
        "aborted",
    )
    runs.append(run)

    run = Run(
        "checkout",
        "sl-resume-aborts-on-removed-child",
        "A Thrift checkout to rev2 fails mid-walk after removing r2's child (r2 "
        "is a directory at rev3, a file at rev2); sl goto to rev2 aborts "
        "listing only that child, which is MISSING_REMOVED.",
        basis=[INTERRUPTED_RESUME, SL_GOTO_CONTRACT],
        type_changes=[{"path": R2, "at_revision": TIP}],
    )
    child = run.layout().type_change_child(R2).path
    _thrift_interrupted(run)
    run.act("a", "d", "checkout", revision=2, via="goto", mode="normal")
    run.call("d", "fail", **sl_abort([child]))
    run.finish()
    sl_abort_rule(
        run,
        "d",
        SATISFIED,
        "a path the interrupted checkout already removed is MISSING_REMOVED, "
        "which blocks",
        "aborted",
    )
    runs.append(run)

    for target in (1, 2):
        same = target == 2
        run = Run(
            "checkout",
            "sl-refused-as-interrupted-" + ("same-target" if same else "other-target"),
            "A Thrift checkout to rev2 fails mid-walk; sl goto to "
            f"rev{target} is refused with the interrupted advice"
            + (" (EdenFS resumes the same target instead)." if same else "."),
            basis=[INTERRUPTED_RESUME],
        )
        _thrift_interrupted(run)
        run.act("a", "d", "checkout", revision=target, via="goto", mode="normal")
        run.call(
            "d",
            "fail",
            **process_fail(255, SL_INTERRUPTED_ADVICE.format(node=2) + "\n"),
        )
        run.finish()
        if same:
            run.expect(
                "checkout.in-progress-rejection",
                "d",
                VIOLATED,
                "only an interrupted checkout to another target is refused",
                identity={"via": "goto"},
            )
        else:
            run.expect(
                "checkout.in-progress-rejection",
                "d",
                SATISFIED,
                "c may have left the mount interrupted at rev2",
            )
        runs.append(run)
    return runs


def timeout_runs() -> list[Run]:
    run3 = _sl_interrupted(
        "interrupted-sl-goto-applies-clean",
        "sl goto to rev2 exits 255 with an EdenError; the clean r0 then reads "
        "rev2's bytes.",
        dirty=False,
    )
    stale_read(run3, "r", R0, SATISFIED, "the daemon may have applied c's r0 effect")
    run4 = _sl_interrupted(
        "interrupted-sl-goto-overwrites-dirty",
        "r0 is written; sl goto to rev2 exits 255 with an EdenError; r0 then reads "
        "rev2's bytes.",
        dirty=True,
    )
    stale_read(
        run4,
        "r",
        R0,
        VIOLATED,
        "neither the NORMAL walk nor a passed DRY_RUN overwrites a dirty changed path",
    )
    return [run3, run4]


def collector_runs() -> list[Run]:
    found: list[Run] = []

    run = _known_interrupted(
        "status-served-before-any-resume-is-violated",
        "A status served before any resume is violated.",
        basis=[
            "edenfs/properties/checkout.py: checkout.conflict-count, checkout.interrupted-refusal, checkout.parent-before-paths, status.thrift-model-stale-status"
        ],
    )
    run.act("a", "c2", "checkout", revision=2, via="thrift", mode="normal")
    run.act("b", "s", "status", channel="thrift", revision=2)
    run.call("s", "ok", **thrift_status({}, revision=2))
    run.call("c2", "ok", **thrift_checkout(2, conflicts=0))
    run.finish()
    run.expect(
        "checkout.conflict-count",
        "c2",
        VIOLATED,
        "no explanation of the run's effects allows the observations",
        identity={"mode": "normal"},
    )
    run.expect(
        "checkout.interrupted-refusal",
        "s",
        VIOLATED,
        "checkout/interrupted-refusal: s was served while c1's interruption stood",
        identity={"action-kind": "status"},
    )
    _start_proof(run, VIOLATED)
    _empty_thrift_status_proof(run)
    found.append(run)

    run = Run(
        "checkout",
        "completed-resume-ends-the-interruption",
        "A later same-target checkout is an ordinary one.",
        basis=[
            "edenfs/properties/checkout.py: bytes.stale-read, checkout.conflict-count, checkout.sl-abort, checkout.parent-before-paths"
        ],
    )
    run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
    run.act("a", "r", "read", path=R2, channel="fuse")
    run.act("a", "l", "readdir", path="tree/registers", channel="thrift")
    run.act("a", "d", "checkout", revision=2, via="goto", mode="normal")
    run.act("a", "e", "checkout", revision=2, via="thrift", mode="normal")
    run.call(
        "c", "fail", reason="thrift: fault", source="thrift", error_type="GENERIC_ERROR"
    )
    run.call("r", "ok", **read_ok(committed(R2, 2)))
    run.call("l", "ok", channel="thrift", entries=["r0.reg", "r1.reg", "r2.reg"])
    run.call("d", "ok", **sl_checkout(2))
    run.call("e", "ok", **thrift_checkout(2, conflicts=0))
    run.finish()
    run.expect("checkout.parent-before-paths", "e", SATISFIED, "no parent before paths")
    found.append(run)

    run = Run(
        "checkout",
        "completed-resume-then-noop-checkout-reports-a-conflict",
        "After the completed resume, the no-op same-target Thrift checkout reports "
        "a conflict.",
        basis=[
            "edenfs/properties/checkout.py: checkout.conflict-count, checkout.parent-before-paths"
        ],
    )
    run.act("a", "c", "checkout", revision=2, via="thrift", mode="normal")
    run.act("a", "r", "read", path=R2, channel="fuse")
    run.act("a", "l", "readdir", path="tree/registers", channel="thrift")
    run.act("a", "d", "checkout", revision=2, via="goto", mode="normal")
    run.act("a", "e", "checkout", revision=2, via="thrift", mode="normal")
    run.call(
        "c", "fail", reason="thrift: fault", source="thrift", error_type="GENERIC_ERROR"
    )
    run.call("r", "ok", **read_ok(committed(R2, 2)))
    run.call("l", "ok", channel="thrift", entries=["r0.reg", "r1.reg", "r2.reg"])
    run.call("d", "ok", **sl_checkout(2))
    run.call("e", "ok", **thrift_checkout(2, conflicts=1))
    run.finish()
    reason = "the resume left every register at rev2, so e finds no conflict"
    run.expect(
        "checkout.conflict-count", "e", VIOLATED, reason, identity={"mode": "normal"}
    )
    run.expect(
        "checkout.parent-before-paths",
        "e",
        VIOLATED,
        reason,
        identity={"via": "thrift"},
    )
    found.append(run)

    for refused in ("status", "checkout"):
        run = Run(
            "checkout",
            f"{refused}-refused-as-interrupted-after-completed-resume",
            "A Thrift checkout to rev2 fails mid-walk; sl goto to rev2 completes "
            "the resume; a later Thrift "
            + ("status" if refused == "status" else "checkout to rev1")
            + " is refused with the interrupted advice.",
            basis=[INTERRUPTED_RESUME],
        )
        _thrift_interrupted(run)
        run.act("a", "d", "checkout", revision=2, via="goto", mode="normal")
        if refused == "status":
            run.act("a", "e", "status", channel="thrift", revision=2)
        else:
            run.act("a", "e", "checkout", revision=1, via="thrift", mode="normal")
        run.call("d", "ok", **sl_checkout(2))
        run.call(
            "e",
            "fail",
            reason=INTERRUPTED_ADVICE.format(node="corpus"),
            source="thrift",
            error_type="CHECKOUT_IN_PROGRESS",
        )
        run.finish()
        reason = "the completed resume ended the interruption before e was invoked"
        if refused == "status":
            run.expect(
                "core.input-legality",
                "e",
                VIOLATED,
                reason,
                subject=run.input_subject("e"),
                identity={"input-kind": "status", "errno": "CHECKOUT_IN_PROGRESS"},
            )
        else:
            run.expect(
                "checkout.in-progress-rejection",
                "e",
                VIOLATED,
                reason,
                identity={"via": "thrift"},
            )
        found.append(run)
    return found


_GENERIC = {
    "reason": "checkout: EdenError: boom",
    "source": "thrift",
    "error_type": "GENERIC_ERROR",
}
_ADVICE_REFUSAL = {
    "reason": INTERRUPTED_ADVICE.format(node="corpus"),
    "source": "thrift",
    "error_type": "CHECKOUT_IN_PROGRESS",
}


def served_while_interrupted_runs() -> list[Run]:
    """Interruptions known from the history rather than from a fault."""
    basis = [CHECKOUT_GUARD, INTERRUPTED_REFUSALS]
    other = Run(
        "checkout",
        "interrupted-advice-then-other-target-served",
        "A Thrift NORMAL to rev2 fails with an EdenError; a status after it is "
        "refused with the interrupted advice; a Thrift NORMAL to rev3 then "
        "succeeds, which the standing interruption forbids.",
        basis=basis,
    )
    other.act("a", "c1", "checkout", revision=2, via="thrift", mode="normal")
    other.act(
        "b", "s1", "status", channel="thrift", revision=2, requires=[("c1", "finished")]
    )
    other.act("b", "c3", "checkout", revision=3, via="thrift", mode="normal")
    other.call("c1", "fail", **_GENERIC)
    other.call("s1", "fail", **_ADVICE_REFUSAL)
    other.call("c3", "ok", **thrift_checkout(3, conflicts=0))
    other.finish()
    other.expect(
        "checkout.interrupted-refusal",
        "c3",
        VIOLATED,
        "s1's advice says c1 left the mount interrupted, which refuses c3",
        identity={"action-kind": "checkout"},
    )
    other.expect(
        "core.input-legality",
        "s1",
        VIOLATED,
        "one core with c3: the advice needs the interruption c3 was served through",
        subject=other.input_subject("s1"),
        identity={"input-kind": "status", "errno": "CHECKOUT_IN_PROGRESS"},
    )

    resumed = _known_interrupted(
        "interrupted-failed-resume-then-status-served",
        "After a known interruption, a same-target Thrift checkout fails before "
        "start; the prior interruption remains and a later status is served.",
        basis=["D129789564: a failed pre-start resume restores the prior interruption"],
    )
    resumed.act(
        "nemesis",
        "f",
        "fault_install",
        fault_id="F",
        site="checkout-begin",
        target_action="c2",
        behavior="error",
        requires=[("started", "finished")],
    )
    resumed.call("f", "ok", fault_id="F")
    resumed.act(
        "a",
        "c2",
        "checkout",
        revision=2,
        via="thrift",
        mode="normal",
        requires=[("f", "finished")],
    )
    resumed.call("c2", "fail", **_GENERIC)
    resumed.act(
        "nemesis", "x", "fault_remove", fault_id="F", requires=[("c2", "finished")]
    )
    resumed.call("x", "ok", fault_id="F", removed=0)
    resumed.act(
        "b", "s", "status", channel="thrift", revision=2, requires=[("x", "finished")]
    )
    resumed.call("s", "ok", **thrift_status({}, 2))
    resumed.fault_removed("F")
    resumed.finish()
    resumed.expect(
        "checkout.interrupted-refusal",
        "s",
        VIOLATED,
        "c2 failed before start, so c1's prior interruption still stands at s",
        identity={"action-kind": "status"},
    )

    _start_proof(resumed, VIOLATED)
    _empty_thrift_status_proof(resumed)

    transport = Run(
        "checkout",
        "interrupted-transport-error-refused-then-served",
        "A Thrift NORMAL to rev2 ends in a transport error; a status after it is "
        "refused with the interrupted advice, so the checkout was interrupted; a "
        "status unordered with that refusal is served.",
        basis=basis,
    )
    transport.act("a", "c1", "checkout", revision=2, via="thrift", mode="normal")
    transport.act(
        "b", "s1", "status", channel="thrift", revision=2, requires=[("c1", "finished")]
    )
    transport.act("b", "s2", "status", channel="thrift", revision=2)
    transport.call("c1", "fail", reason="thrift: transport closed", source="thrift")
    transport.call("s1", "fail", **_ADVICE_REFUSAL)
    transport.call("s2", "ok", **thrift_status({}, 2))
    transport.finish()
    transport.expect(
        "checkout.interrupted-refusal",
        "s2",
        VIOLATED,
        "s1's advice needs c1 interrupted from before s1 to after s2",
        identity={"action-kind": "status"},
    )
    transport.expect(
        "checkout.interrupted-refusal",
        "c1",
        VIOLATED,
        "one core with s2: c1 is interrupted only after its parent group",
        identity={"action-kind": "checkout"},
    )
    transport.expect(
        "core.input-legality",
        "s1",
        VIOLATED,
        "one core with s2: the advice needs c1's interruption",
        subject=transport.input_subject("s1"),
        identity={"input-kind": "status", "errno": "CHECKOUT_IN_PROGRESS"},
    )
    completed = _known_interrupted(
        "interrupted-resume-then-status-served",
        "After a proven interruption, a same-target Thrift resume completes; "
        "a subsequent status is served legally.",
        basis=["a completed same-target resume ends the interruption"],
    )
    completed.act("a", "c2", "checkout", revision=2, via="thrift", mode="normal")
    completed.call("c2", "ok", **thrift_checkout(2, conflicts=0))
    completed.act(
        "a",
        "s",
        "status",
        channel="thrift",
        revision=2,
        requires=[("c2", "finished")],
    )
    completed.call("s", "ok", **thrift_status({}, 2))
    completed.finish()
    _start_proof(completed, SATISFIED)
    completed.expect(
        "checkout.interrupted-refusal",
        "s",
        SATISFIED,
        "the completed same-target resume ended c1's interruption before status",
        identity={"action-kind": "status"},
    )
    return [other, resumed, transport, completed]


def runs() -> list[Run]:
    return [
        *_interrupted_runs(),
        *interrupted_runs(),
        *served_while_interrupted_runs(),
        *timeout_runs(),
        *resume_and_continue_runs(),
        *collector_runs(),
    ]
