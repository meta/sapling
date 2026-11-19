# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""EdenFS action vocabulary: payload schemas, strict result schemas, and static effects.

Each action kind declares its payload validator and sampler, the exact
result fields of every outcome with the errnos it may fail with, the
modeled paths it may touch, and what its effects can be. A RunSpec is also
validated as a whole, once: its environment, its write values, and, for a
differential run, the shared action subset in a comparable schedule.
"""

from __future__ import annotations

import dataclasses
import hashlib
import math
import random
from collections.abc import Callable, Mapping
from pathlib import PurePosixPath
from typing import Any, TYPE_CHECKING

from eden.integration.correctness.exploratory.core.actions import (
    _sample_none,
    compose_action_defs,
    CORE_ACTION_DEFS,
    write_sampler,
)
from eden.integration.correctness.exploratory.core.differential import REFERENCE_RESULT
from eden.integration.correctness.exploratory.core.differential_schedule import (
    Applicability,
    not_applicable,
    ObservationDef,
    SHARED,
    validate_differential,
)
from eden.integration.correctness.exploratory.core.effects import (
    Footprint,
    footprints_interact,
    normalize_path,
    ROOT,
)
from eden.integration.correctness.exploratory.core.facts import (
    ErrnoUse,
    FieldKind,
    FieldSpec,
    NO_ERRNO,
    paired,
    ResultSchema,
)
from eden.integration.correctness.exploratory.core.runspec import (
    Action,
    finish_before,
    may_overlap,
    RunSpec,
    SpecError,
)
from eden.integration.correctness.exploratory.core.target import (
    ActionDef,
    Binding,
    GenerationOptions,
    Grouping,
    HeldDescriptor,
    HeldInput,
    Role,
)
from eden.integration.correctness.exploratory.edenfs.encoding import (
    encode_committed,
    encode_value,
    initial_content,
    is_reserved_writer,
)
from eden.integration.correctness.exploratory.edenfs.fixture import (
    CHURN_FILES,
    in_layout,
    layout_for,
)
from eden.integration.correctness.exploratory.edenfs.observation_bounds import (
    validate_observation_sizes,
)
from eden.integration.correctness.exploratory.edenfs.sites import (
    CHECKOUT_BEGIN_SITE,
    FETCH_TREE_SITE,
    LOOKUP_SITE,
    observable_target_kinds,
    STATUS_CACHE_FILL_SITE,
    TREE_CHECKOUT_KEY,
    TREE_CHECKOUT_SITE,
)
from eden.integration.correctness.repo import MIN_BLOB_SIZE

if TYPE_CHECKING:
    from eden.integration.correctness.exploratory.core.facts import HistoryFacts

SYNC_TIMEOUTS = frozenset({-1, 0, 60})
CHECKOUT_NORMAL = "normal"
CHECKOUT_FORCE = "force"
# `sl go --continue`: resume the interrupted goto `.hg/updatestate` names.
# `revision` is the destination the run expects it to name.
CHECKOUT_CONTINUE = "continue"
CHECKOUT_MODES = frozenset({CHECKOUT_NORMAL, CHECKOUT_FORCE, CHECKOUT_CONTINUE})
SL_CHECKOUT_VIAS = frozenset({"goto"})
# A sampled checkout is forced this often unless a process kill needs sl;
# with the repeated-producer tails' forced departures, the overall forced
# share of checkouts stays near what composition without tails produced.
FORCE_CHECKOUT_RATE = 0.2
# Of the normal checkouts that need not spawn sl, this share goes via Thrift.
THRIFT_NORMAL_RATE = 0.15
# A sampled checkout returns to the initial tip or an already composed
# checkout's revision this often; otherwise it picks an unvisited revision.
REVISIT_RATE = 0.125
# After a checkout an injected error or a process kill may have interrupted,
# a sampled checkout resumes it with `sl go --continue` this often (the
# resume terminates on EdenFS).
CONTINUE_RATE = 0.5
CHMOD_MODES = (0o644, 0o755)
# Each namespace kind (rename, unlink, mkdir, sl rm, sl mv) offers a sample
# at this rate, so together they weigh about one other input kind; a rename
# replaces another register at MOVE_ONTO_REGISTER_RATE (prior destination
# removed), else it names a new path; an sl mv always names a new path.
NAMESPACE_SAMPLE_RATE = 0.25
MOVE_ONTO_REGISTER_RATE = 0.5
MOVE_KINDS = frozenset({"rename", "sl_mv"})
_OPTIONAL_SETUP = frozenset(
    {"memory_max_mb", "warm_readdir", "aged_overlay", "wide_dir", "reference_clone"}
)
READDIR_CHANNELS = frozenset({"fuse", "thrift"})
# A plain Linux reference checkout is case-sensitive; a case-insensitive
# mount resolves names it cannot, so no pair would compare one operation.
CASE_INSENSITIVE_MOUNT = "case-insensitive-mount"
HELD_WIDE_DIR = {"name": "tree/wide", "entries": 2000, "churn_all": True}
# A descriptor held across one actor's inputs, in a quarter of the specs
# composed without a fault stage (plain no-fault and differential runs).
HELD_DESCRIPTOR = HeldDescriptor(
    open="open", read="read_fd", close="close", field="fd", probability=0.25
)
HELD_INPUTS = (
    HeldInput(
        stream="readdir_stream",
        resume="readdir_resume",
        partner="checkout",
        probability=0.5,
        setup_profile={"wide_dir": HELD_WIDE_DIR},
        stream_payload={
            "path": HELD_WIDE_DIR["name"],
            "pause_after": HELD_WIDE_DIR["entries"] // 2,
        },
        resume_field="stream",
    ),
)


def repository(spec: RunSpec) -> Mapping[str, Any]:
    """Return the validated repository payload."""
    value = spec.environment.target["repository"]
    if not isinstance(value, dict):
        raise SpecError("environment.target.repository must be an object")
    return value


def _require_exact(
    raw: object,
    allowed: frozenset[str],
    what: str,
    optional: frozenset[str] = frozenset(),
) -> dict[str, Any]:
    if type(raw) is not dict:
        raise SpecError(f"{what} must be an object")
    unknown = sorted(set(raw) - allowed)
    if unknown:
        raise SpecError(f"{what} has unknown fields: {unknown}")
    missing = sorted(allowed - optional - set(raw))
    if missing:
        raise SpecError(f"{what} is missing fields: {missing}")
    return raw


def _validate_environment(spec: RunSpec) -> None:
    env = spec.environment.target
    _validate_repository(env["repository"])
    _validate_setup_profile(
        env["setup_profile"],
        env["repository"]["file_count"],
        env["repository"]["commit_count"],
    )
    if env["channel"] != "fuse":
        raise SpecError("environment.target.channel is unsupported")
    if env["fixture_source"] != "backing":
        raise SpecError("environment.target.fixture_source is unsupported")
    if env["setup_profile"].get("reference_clone", False) and not env["case_sensitive"]:
        raise SpecError(
            f"setup_profile.reference_clone is not applicable: {CASE_INSENSITIVE_MOUNT}"
        )
    _validate_registers(spec)


# Every generated file and generated directory is one of these or under one
# (`repo.GeneratedRepo.file_at`), at every revision and repository shape.
GENERATED_DIRECTORIES = (("tree", "a0"), ("tree", "a1"))


def _validate_registers(spec: RunSpec) -> None:
    """Refuse a register that another class of fixture path also claims.

    A register holds revision-specific committed images, so it must be a
    file path of its own: not `tree`, not a generated file or directory
    (all of them sit under `tree/a0` and `tree/a1`, so nothing under
    those is a register either), and neither equal to, an ancestor of,
    nor under another register (a type-changing register included), a
    churn path, or the wide directory. The check reads paths only: it
    never builds the layout.
    """
    env = spec.environment.target
    registers = env["repository"]["registers"]
    claims = [(path, "register") for path in registers]
    claims += [(path, "status-only churn path") for path in CHURN_FILES]
    wide = env["setup_profile"].get("wide_dir")
    if wide is not None:
        claims.append((wide["name"], "wide directory"))
    for path in registers:
        parts = PurePosixPath(path).parts
        if "/".join(parts) != path:
            raise SpecError(f"register {path!r} is not a canonical path")
        if parts == ("tree",) or parts[:2] in GENERATED_DIRECTORIES:
            raise SpecError(
                f"register {path!r} is a generated path or directory, or under one"
            )
    for index, path in enumerate(registers):
        parts = PurePosixPath(path).parts
        for other_index, (other, what) in enumerate(claims):
            other_parts = PurePosixPath(other).parts
            shorter = min(len(parts), len(other_parts))
            if other_index != index and parts[:shorter] == other_parts[:shorter]:
                raise SpecError(f"register {path!r} overlaps {what} {other!r}")


# A status observation's rows (`properties/status.py` `_diff_rows`) grow as
# the cube of the candidate revisions; the sampler, corpus and decks use at
# most 4 commits.
MAX_COMMIT_COUNT = 6


def _validate_repository(raw: object) -> None:
    """Validate the target-owned generated repository description."""
    repo = _require_exact(
        raw,
        frozenset(
            {"file_count", "commit_count", "registers", "type_changes", "blob_size"}
        ),
        "environment.target.repository",
        frozenset({"type_changes", "blob_size"}),
    )
    floors = {"file_count": 1, "commit_count": 1, "blob_size": MIN_BLOB_SIZE}
    for name, floor in floors.items():
        if name in repo and (type(repo[name]) is not int or repo[name] < floor):
            raise SpecError(
                f"environment.target.repository.{name} must be at least {floor}"
            )
    if repo["commit_count"] > MAX_COMMIT_COUNT:
        raise SpecError(
            "environment.target.repository.commit_count must be at most "
            f"{MAX_COMMIT_COUNT}"
        )
    paths = repo["registers"]
    if type(paths) is not list or not paths:
        raise SpecError("environment.target.repository.registers must be non-empty")
    for path_value in paths:
        if type(path_value) is not str or not path_value:
            raise SpecError("environment target path must be a non-empty string")
        path = PurePosixPath(path_value)
        if path.is_absolute() or ".." in path.parts:
            raise SpecError(f"path {path_value!r} escapes the mount")
    if type(repo.get("type_changes", [])) is not list:
        raise SpecError("environment.target.repository.type_changes must be a list")
    for change in repo.get("type_changes", []):
        body = _require_exact(
            change, frozenset({"path", "at_revision"}), "repository.type_changes entry"
        )
        if body["path"] not in paths:
            raise SpecError("type_changes path must be one of repository.registers")
        revision = body["at_revision"]
        if type(revision) is not int or not 1 <= revision < repo["commit_count"]:
            raise SpecError("type_changes at_revision must name a later fixture commit")


def _validate_setup_profile(raw: object, file_count: int, commit_count: int) -> None:
    """Validate target setup choices that the current environment supports."""
    profile = _require_exact(
        raw,
        _OPTIONAL_SETUP | frozenset({"materialize", "scatter_dirty"}),
        "environment.target.setup_profile",
        _OPTIONAL_SETUP,
    )
    if "wide_dir" in profile:
        _validate_wide_dir(profile["wide_dir"])
    if type(profile.get("reference_clone", False)) is not bool:
        raise SpecError("setup_profile.reference_clone must be bool")
    if type(profile["materialize"]) is not bool:
        raise SpecError("setup_profile.materialize must be bool")
    if (
        type(profile["scatter_dirty"]) is not int
        or not 0 <= profile["scatter_dirty"] <= file_count
    ):
        raise SpecError(
            f"setup_profile.scatter_dirty must be between 0 and {file_count}"
        )
    if profile.get("memory_max_mb") is not None and (
        type(profile["memory_max_mb"]) is not int or profile["memory_max_mb"] < 1
    ):
        raise SpecError("setup_profile.memory_max_mb must be positive")
    if "warm_readdir" in profile and type(profile["warm_readdir"]) is not bool:
        raise SpecError("setup_profile.warm_readdir must be bool")
    if "aged_overlay" in profile and (
        type(profile["aged_overlay"]) is not int
        or not 0 <= profile["aged_overlay"] < commit_count
    ):
        raise SpecError(
            f"setup_profile.aged_overlay must be between 0 and {commit_count - 1}"
        )


def _validate_mount_relative(value: object, what: str) -> None:
    path = PurePosixPath(str(value))
    if type(value) is not str or not value or path.is_absolute() or ".." in path.parts:
        raise SpecError(f"{what} must be mount-relative")


def _validate_wide_dir(raw: object) -> None:
    wide = _require_exact(
        raw, frozenset({"name", "entries", "churn_all"}), "setup_profile.wide_dir"
    )
    _validate_mount_relative(wide["name"], "setup_profile.wide_dir.name")
    if type(wide["entries"]) is not int or wide["entries"] < 1:
        raise SpecError("setup_profile.wide_dir.entries must be positive")
    if type(wide["churn_all"]) is not bool:
        raise SpecError("setup_profile.wide_dir.churn_all must be bool")


def wide_dir(spec: RunSpec) -> Mapping[str, Any] | None:
    """Return the validated wide directory knob, if the setup profile sets one."""
    return spec.environment.target["setup_profile"].get("wide_dir")


class ReferenceNotApplicable(RuntimeError):
    """A differential run whose environment cannot pair with a plain checkout."""

    def __init__(self, reason: str) -> None:
        super().__init__(reason)
        self.reason = reason


def reference_clone(spec: RunSpec) -> bool:
    """Return whether the run is differential: setup adds a reference checkout.

    The reference checkout is a plain filesystem clone of the fixture. It is
    never an observation channel; each shared action runs on it as well and
    reports its outcome inside the action's terminal event.
    """
    return spec.environment.target["setup_profile"].get("reference_clone", False)


def _sample_path(rng: random.Random, context: Mapping[str, Any]) -> str:
    registers = context["environment"]["repository"]["registers"]
    return registers[0] if context.get("contended") else rng.choice(registers)


def _sample_nonwrite(context: Mapping[str, Any]) -> bool:
    return bool(
        context.get("forced_kind")
        or "input_index" not in context
        or not context.get("contended")
    )


def _sample_read(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    if not _sample_nonwrite(context):
        return None
    channel = "thrift" if context.get("site") == "wait-for-pending-writes" else "fuse"
    result: dict[str, Any] = {"path": _sample_path(rng, context), "channel": channel}
    if channel == "thrift":
        result["sync_timeout_seconds"] = rng.choice((-1, 60))
    return result


def _sample_getattr(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    return {"path": _sample_path(rng, context)} if _sample_nonwrite(context) else None


def _sample_chmod(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    if not _sample_nonwrite(context):
        return None
    return {"path": _sample_path(rng, context), "mode": rng.choice(CHMOD_MODES)}


def _offers(rng: random.Random, kind: str) -> bool:
    """Return whether a namespace kind offers a sample, without advancing `rng`.

    The draw hashes the stream's state with the kind, so a declining
    sampler leaves the stream, and the rest of the composition, exactly as
    without it.
    """
    digest = hashlib.sha256(repr((rng.getstate(), kind)).encode()).digest()
    return int.from_bytes(digest[:8]) < NAMESPACE_SAMPLE_RATE * 2**64


def _namespace_sample(
    rng: random.Random, context: Mapping[str, Any], kind: str
) -> str | None:
    """Return the register a namespace sample names, or None when it declines.

    Only a composed input actor's sample may be a namespace operation: a
    forced kind (baseline input, fault target, kill target, repeated-producer
    stage) declines, so those keep their calibrated mixes.
    """
    if context.get("forced_kind") or not _sample_nonwrite(context):
        return None
    if not _offers(rng, kind):
        return None
    return _sample_path(rng, context)


def _new_path(register: str, prefix: str, context: Mapping[str, Any]) -> str:
    """Return a path no fixture holds, beside `register` (a known directory)."""
    return f"{PurePosixPath(register).parent}/{prefix}-{context['action_id']}"


def _remove_sampler(
    kind: str,
) -> Callable[[random.Random, Mapping[str, Any]], Mapping[str, Any] | None]:
    """Return an unlink or `sl rm` sampler."""

    def sample(
        rng: random.Random, context: Mapping[str, Any]
    ) -> Mapping[str, Any] | None:
        """Sample a removal of a register."""
        register = _namespace_sample(rng, context, kind)
        return None if register is None else {"path": register}

    return sample


def _sample_mkdir(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Sample a mkdir of a new path beside a register."""
    register = _namespace_sample(rng, context, "mkdir")
    return None if register is None else {"path": _new_path(register, "dir", context)}


def _move_sampler(
    kind: str,
) -> Callable[[random.Random, Mapping[str, Any]], Mapping[str, Any] | None]:
    """Return a move sampler; a rename may also replace another register."""
    onto_register = kind == "rename"

    def sample(
        rng: random.Random, context: Mapping[str, Any]
    ) -> Mapping[str, Any] | None:
        """Sample a move of a register to a new path or onto another register."""
        source = _namespace_sample(rng, context, kind)
        if source is None:
            return None
        registers = context["environment"]["repository"]["registers"]
        others = [path for path in registers if path != source]
        if onto_register and others and rng.random() < MOVE_ONTO_REGISTER_RATE:
            return {"source": source, "destination": rng.choice(others)}
        return {"source": source, "destination": _new_path(source, "moved", context)}

    return sample


def _sample_status(
    _rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    if not _sample_nonwrite(context):
        return None
    if context.get("site") == "fetch-tree":
        count = context["environment"]["repository"]["commit_count"]
        return {"channel": "thrift", "revision": count - 1}
    return {"channel": "sl"}


def _checkout_revision(
    rng: random.Random, context: Mapping[str, Any], count: int
) -> int:
    """Pick a revision below `count`, revisiting one at `REVISIT_RATE`.

    A revisit returns to the initial tip or to an already composed
    checkout's revision, so organic repeated producers occur anywhere in
    the spec, inside fault windows included. Otherwise the revision is one
    neither the tip nor a composed checkout names, falling back to any
    non-tip revision once those run out.
    """
    tip = context["environment"]["repository"]["commit_count"] - 1
    composed = context.get("actions", {})
    visited = {tip} | {
        action.payload["revision"]
        for action in composed.values()
        if action.kind == "checkout"
    }
    revisit = rng.random() < REVISIT_RATE
    seen = [revision for revision in range(count) if revision in visited]
    if revisit and seen:
        return rng.choice(seen)
    fresh = [revision for revision in range(count) if revision not in visited]
    return rng.choice(
        fresh or [revision for revision in range(count) if revision != tip] or [tip]
    )


def _declines_fault_window(window: Mapping[str, Any]) -> bool:
    """Return whether a repeated-producer tail must stay out of this fault window.

    Fetch-tree and fuse-lookup reserve every tree-wide action until the
    fault is removed (their site validators reject one), and a parked
    checkout target holds the checkout lock, so tail checkouts could only
    fail with CHECKOUT_IN_PROGRESS.
    """
    if window["site"] in (FETCH_TREE_SITE, LOOKUP_SITE):
        return True
    return window["behavior"] == "block" and window["target_action"].kind == "checkout"


def _sample_revisit(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Depart from the tip with a forced Thrift checkout, or return to it.

    Both legs go via Thrift: Sapling's parent stays at the tip after a
    Thrift checkout, so an `sl goto` back to the tip would be a no-op.
    The forced departure leaves nothing dirty, so the normal return applies.
    None declines a fault window `_declines_fault_window` rejects.
    """
    window = context.get("fault_window")
    if window is not None and _declines_fault_window(window):
        return None
    count = context["environment"]["repository"]["commit_count"]
    tip = count - 1
    if context["producer_stratum"] == "depart":
        return {
            "revision": rng.randrange(tip) if tip else tip,
            "via": "thrift",
            "mode": CHECKOUT_FORCE,
        }
    return {"revision": tip, "via": "thrift", "mode": CHECKOUT_NORMAL}


def _interruptible_checkout(context: Mapping[str, Any]) -> Action | None:
    """Return a composed checkout an `error` fault or a process kill targets, if any.

    Either may interrupt it on any channel: an error in `sl goto`'s
    DRY_RUN leaves no `.hg/updatestate`, one mid-walk or a kill after
    DRY_RUN leaves it, and a Thrift checkout never writes it.
    """
    composed = context.get("actions", {})
    targets = {
        action.payload.get("target_action")
        for action in composed.values()
        if action.kind == "kill_process"
        or (
            action.kind == "fault_install" and action.payload.get("behavior") == "error"
        )
    }
    return next(
        (
            action
            for action in composed.values()
            if action.kind == "checkout"
            and action.id in targets
            and action.payload["mode"] != CHECKOUT_CONTINUE
        ),
        None,
    )


def _sample_checkout(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Sample a checkout; `_checkout_revision` picks its revision.

    `FORCE_CHECKOUT_RATE` are forced via Thrift (which reports the
    conflict count; `sl goto --clean` is not sampled) unless a process kill
    needs sl; of the rest, `THRIFT_NORMAL_RATE` go via Thrift NORMAL (typed
    per-path conflicts) and the others via `sl goto` (global abort). In
    a differential environment every checkout is a NORMAL `sl goto` (the
    draws still happen, so other choices keep their stream). A
    ``kill_process`` target must spawn an sl process, so a spawning sample
    is always a NORMAL `sl goto`. After a checkout a fault or kill may have
    interrupted, `CONTINUE_RATE` resume it (`_interruptible_checkout`).
    """
    if context.get("producer_stratum") in ("depart", "return"):
        return _sample_revisit(rng, context)
    if not _sample_nonwrite(context):
        return None
    interrupted = _interruptible_checkout(context)
    sl_revisions = {
        action.payload["revision"]
        for action in context.get("actions", {}).values()
        if action.kind == "checkout" and action.payload["via"] in SL_CHECKOUT_VIAS
    }
    if (
        interrupted is not None
        and rng.random() < CONTINUE_RATE
        and sl_revisions <= {interrupted.payload["revision"]}
    ):
        return {
            "revision": interrupted.payload["revision"],
            "via": "goto",
            "mode": CHECKOUT_CONTINUE,
        }
    count = context["environment"]["repository"]["commit_count"]
    spawning = bool(context.get("spawning"))
    force = not spawning and rng.random() < FORCE_CHECKOUT_RATE
    thrift_normal = not spawning and not force and rng.random() < THRIFT_NORMAL_RATE
    if context["environment"]["setup_profile"].get("reference_clone"):
        # A differential run shares only `sl goto` in NORMAL mode.
        force = thrift_normal = False
    via = (
        "thrift"
        if force or thrift_normal or context.get("site") == TREE_CHECKOUT_SITE
        else "goto"
    )
    if context.get("forced_attempt") and context.get("site") == TREE_CHECKOUT_SITE:
        if count < 2:
            raise ValueError("forced tree-checkout requires a non-tip revision")
        count -= 1
    mode = CHECKOUT_FORCE if force else CHECKOUT_NORMAL
    revision = _checkout_revision(rng, context, count)
    continued = [
        action.payload["revision"]
        for action in context.get("actions", {}).values()
        if action.kind == "checkout" and action.payload["mode"] == CHECKOUT_CONTINUE
    ]
    if continued and via != "thrift":
        # A continue goes where `.hg/updatestate` points (`_validate_checkout`).
        revision = continued[0]
    return {
        "revision": revision,
        "via": via,
        "mode": mode,
    }


def _sample_open(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Sample the open of a held descriptor (`HELD_DESCRIPTOR`); decline otherwise."""
    if not context.get("held_descriptor"):
        return None
    return {"path": _sample_path(rng, context)}


def _sample_readdir(
    rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Sample a FUSE listing of the registers' directory, in a differential run only.

    Plain composition declines without a draw.
    """
    profile = context.get("environment", {}).get("setup_profile", {})
    if not profile.get("reference_clone"):
        return None
    if not _sample_nonwrite(context):
        return None
    directory = str(PurePosixPath(_sample_path(rng, context)).parent)
    return {"path": directory, "channel": "fuse"}


def _sample_background_read(
    _rng: random.Random, _context: Mapping[str, Any]
) -> Mapping[str, Any]:
    """Return one short finite directory-read lease for generated runs."""
    return {"root": "tree", "seconds": 2.0}


def _sample_observation_pair(
    rng: random.Random, context: Mapping[str, Any]
) -> tuple[tuple[str, Mapping[str, Any]], ...] | None:
    """Occasionally observe one register consecutively through both read channels."""
    if rng.randrange(4):
        return None
    paths = list(context["environment"]["repository"]["registers"])
    install = context["install"]
    if install.payload["site"] == LOOKUP_SITE:
        target = context["actions"][install.payload["target_action"]]
        paths = [path for path in paths if path != target.payload.get("path")]
    if not paths:
        return None
    path = rng.choice(paths)
    return (
        ("read", {"path": path, "channel": "fuse"}),
        (
            "read",
            {
                "path": path,
                "channel": "thrift",
                "sync_timeout_seconds": rng.choice((0, 60)),
            },
        ),
    )


def _configure_forced_attempt(
    configure: Callable[..., object], context: Mapping[str, Any]
) -> None:
    """Bind a forced attempt's target kind and setup profile to its site."""
    site_name = context["site"]
    site = context["site_definition"]
    target_kind = "read" if site_name == LOOKUP_SITE else sorted(site.target_kinds)[0]
    environment = context["environment"]
    setup_profile = dict(environment["setup_profile"])
    if site_name in {
        CHECKOUT_BEGIN_SITE,
        FETCH_TREE_SITE,
        LOOKUP_SITE,
    }:
        setup_profile["materialize"] = False
    elif site_name == TREE_CHECKOUT_SITE:
        setup_profile["materialize"] = True
    if target_kind == "checkout" or site_name in {
        LOOKUP_SITE,
        STATUS_CACHE_FILL_SITE,
    }:
        setup_profile["scatter_dirty"] = 0
    if site_name == LOOKUP_SITE:
        setup_profile["warm_readdir"] = False
        setup_profile["aged_overlay"] = 0
    configure(
        environment={**environment, "setup_profile": setup_profile},
        target_kind=target_kind,
        mid_flight=False,
        observation_placement="post",
        serialize_sibling_inputs=site_name == STATUS_CACHE_FILL_SITE,
    )


def sample_fault_install(
    _rng: random.Random, context: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Sample EdenFS fields for the existing core fault-install schema."""
    configure = context.get("configure_forced_attempt")
    if callable(configure):
        _configure_forced_attempt(configure, context)
    configure_composition = context.get("configure_composition")
    if callable(configure_composition) and context["site"] == LOOKUP_SITE:
        environment = context["environment"]
        setup_profile = dict(environment["setup_profile"])
        setup_profile["warm_readdir"] = False
        setup_profile["aged_overlay"] = 0
        configure_composition(
            environment={**environment, "setup_profile": setup_profile}
        )
    result: dict[str, Any] = {
        "fault_id": "F",
        "site": context["site"],
        "target_action": context["target_action"],
        "behavior": context["behavior"],
    }
    if context["site"] == "tree-checkout":
        result.update({"key": TREE_CHECKOUT_KEY, "ordinal": 1})
    if context["behavior"] == "delay":
        result["delay_ms"] = 100
    return result


def _validate_path(spec: RunSpec, action: Action, *, allow_churn: bool = False) -> None:
    path = action.payload["path"]
    if path in CHURN_FILES:
        if allow_churn:
            return
        raise SpecError(f"path {path!r} is status-only")
    if path not in repository(spec)["registers"]:
        raise SpecError(f"path {path!r} is not in repository.registers")


def model_knows(spec: RunSpec, path: str, before: Action) -> bool:
    """Return whether an observer may name ``path`` when ``before`` starts.

    Registers and churn files exist from setup. A ``mkdir`` path and a
    rename's destination or source (ENOENT is the source's legal set) are
    known once the producing action must finish before ``before`` starts,
    by actor order or a finished ``requires`` edge, transitively. A path
    only a later or unordered action produces is unknown.
    """
    if path in (*repository(spec)["registers"], *CHURN_FILES):
        return True
    earlier = finish_before(spec, before.id)
    for action in spec.actions:
        if action.id not in earlier:
            continue
        if action.kind == "mkdir" and action.payload["path"] == path:
            return True
        if action.kind in MOVE_KINDS and path in action.payload.values():
            return True
    return False


def _in_tip_layout(spec: RunSpec, path: str) -> bool:
    """Return whether the generated repository has ``path`` at its tip revision.

    One indexed fixture query: the path grammar names its index.
    """
    tip = repository(spec)["commit_count"] - 1
    return in_layout(layout_for(spec), path, tip)


def _validate_observer_path(
    spec: RunSpec, action: Action, *, allow_layout: bool = False
) -> None:
    """Require an observed path the model knows; churn files stay status-only."""
    path = action.payload["path"]
    if path in CHURN_FILES:
        raise SpecError(f"path {path!r} is status-only")
    if model_knows(spec, path, action) or (allow_layout and _in_tip_layout(spec, path)):
        return
    raise SpecError(f"path {path!r} is not known to the model before {action.id!r}")


def _validate_getattr(spec: RunSpec, action: Action) -> None:
    _validate_observer_path(spec, action, allow_layout=True)


def _validate_chmod(spec: RunSpec, action: Action) -> None:
    """Require a model-known path no mkdir names, and one of the two sampled modes.

    Modeled: the daemon serves the chmod on harness-authored
    state, so a failure outside every nemesis window is the daemon's unless
    the model left the path absent (ENOENT is then in its legal set). The
    model has no directory permissions: a chmod 0644 of a directory drops
    its search bit, and a later walk below it fails EACCES, which no
    modeled state explains.
    """
    _validate_observer_path(spec, action)
    if any(
        other.kind == "mkdir" and other.payload["path"] == action.payload["path"]
        for other in spec.actions
    ):
        raise SpecError(f"chmod path {action.payload['path']!r} is a mkdir path")
    if any(
        change["path"] == action.payload["path"]
        for change in repository(spec).get("type_changes", [])
    ):
        raise SpecError(f"chmod path {action.payload['path']!r} changes type")
    if action.payload["mode"] not in CHMOD_MODES:
        raise SpecError(f"chmod mode must be one of {[oct(m) for m in CHMOD_MODES]}")


def _validate_write(spec: RunSpec, action: Action) -> None:
    _validate_path(spec, action, allow_churn=True)


def _validate_read(spec: RunSpec, action: Action) -> None:
    _validate_observer_path(spec, action)
    channel = action.payload["channel"]
    timeout_present = "sync_timeout_seconds" in action.payload
    if channel not in {"fuse", "thrift"}:
        raise SpecError(
            f"action {action.id!r} has unsupported read channel {channel!r}"
        )
    if channel == "thrift" and not timeout_present:
        raise SpecError("thrift read requires sync_timeout_seconds")
    if channel == "fuse" and timeout_present:
        raise SpecError("fuse read rejects sync_timeout_seconds")
    if timeout_present and action.payload["sync_timeout_seconds"] not in SYNC_TIMEOUTS:
        raise SpecError(f"sync_timeout_seconds must be one of {sorted(SYNC_TIMEOUTS)}")


def _validate_status(spec: RunSpec, action: Action) -> None:
    if action.payload["channel"] not in {"sl", "thrift"}:
        raise SpecError("status channel must be 'sl' or 'thrift'")
    revision = action.payload.get("revision")
    if revision is not None and not 0 <= revision < repository(spec)["commit_count"]:
        raise SpecError(f"action {action.id!r} revision {revision} out of range")
    if action.payload["channel"] != "sl" and any(
        other.kind == "kill_process" and other.payload.get("target_action") == action.id
        for other in spec.actions
    ):
        raise SpecError("process-kill target status must spawn an sl process")


def _validate_checkout(spec: RunSpec, action: Action) -> None:
    if action.payload["via"] not in {"goto", "thrift"}:
        raise SpecError("checkout via must be 'goto' or 'thrift'")
    if action.payload["via"] not in {"goto"} and any(
        other.kind == "kill_process" and other.payload.get("target_action") == action.id
        for other in spec.actions
    ):
        raise SpecError("process-kill target checkout must spawn an sl process")
    revision = action.payload["revision"]
    if not 0 <= revision < repository(spec)["commit_count"]:
        raise SpecError(f"action {action.id!r} revision {revision} out of range")
    mode = action.payload["mode"]
    if mode not in CHECKOUT_MODES:
        raise SpecError(f"checkout mode must be one of {sorted(CHECKOUT_MODES)}")
    if mode == CHECKOUT_CONTINUE and action.payload["via"] not in SL_CHECKOUT_VIAS:
        raise SpecError("checkout mode 'continue' requires via 'goto'")
    if mode == CHECKOUT_CONTINUE and any(
        other.kind == "checkout"
        and other.payload["via"] in SL_CHECKOUT_VIAS
        and other.payload["mode"] != CHECKOUT_CONTINUE
        and other.payload["revision"] != revision
        and action.id not in finish_before(spec, other.id)
        for other in spec.actions
    ):
        # `sl goto --continue` goes where `.hg/updatestate` points, and any
        # other `sl` attempt may leave its own destination there.
        raise SpecError(
            "a checkout --continue requires every sl checkout to another revision"
            " to start after it finishes"
        )


def _validate_manifest_presence(spec: RunSpec, action: Action) -> None:
    """Require both revisions in the fixture's range, like a checkout's."""
    count = repository(spec)["commit_count"]
    for field in ("from_revision", "to_revision"):
        if not 0 <= action.payload[field] < count:
            raise SpecError(
                f"action {action.id!r} {field} {action.payload[field]} out of range"
            )


def _validate_environment_only(_spec: RunSpec, _action: Action) -> None:
    """Accept any payload: run validation already checked the environment."""


def _kill_waits_for_reader(spec: RunSpec, reader: Action, killer: Action) -> bool:
    """Follow event dependencies, including started edges and actor sequencing."""
    edges = {}
    for action in spec.actions:
        edges[action.id, "started"] = set(action.requires)
        edges[action.id, "finished"] = {(action.id, "started")}
    for actor in spec.actors:
        for previous, following in zip(actor.actions, actor.actions[1:]):
            edges[following, "started"].add((previous, "finished"))
    pending = [(killer.id, "started")]
    seen = set()
    while pending:
        event = pending.pop()
        if event == (reader.id, "finished"):
            return True
        if event not in seen:
            seen.add(event)
            pending.extend(edges[event])
    return False


def _validate_background_read(spec: RunSpec, action: Action) -> None:
    """Require a mount-relative root and a bounded or independently killed lease."""
    root = PurePosixPath(action.payload["root"])
    if not action.payload["root"] or root.is_absolute() or ".." in root.parts:
        raise SpecError("background_read root must be mount-relative")
    seconds = action.payload["seconds"]
    if type(seconds) not in (int, float) or not math.isfinite(seconds) or seconds <= 0:
        raise SpecError("background_read seconds must be positive and finite")
    budget = min(
        spec.budgets.operation_deadline_seconds, spec.budgets.runner_deadline_seconds
    )
    if seconds < budget:
        return
    if not any(
        other.kind == "kill_process"
        and other.payload.get("target_action") == action.id
        and 0 <= other.payload["after_ms"] < budget * 1000
        and not _kill_waits_for_reader(spec, action, other)
        for other in spec.actions
    ):
        raise SpecError("background_read lease exceeds budget without a reachable kill")


def _validate_readdir_stream(spec: RunSpec, action: Action) -> None:
    """Require the wide directory as path, a positive pause, and one resume.

    The readdir property only models the generated wide directory's listing,
    so no other directory may be streamed.
    """
    _validate_mount_relative(action.payload["path"], "readdir_stream path")
    wide = wide_dir(spec)
    if wide is None:
        raise SpecError("readdir_stream requires setup_profile.wide_dir")
    if action.payload["path"] != wide["name"]:
        raise SpecError(
            f"readdir_stream path must be setup_profile.wide_dir.name {wide['name']!r}"
        )
    if (
        type(action.payload["pause_after"]) is not int
        or action.payload["pause_after"] < 1
    ):
        raise SpecError("readdir_stream pause_after must be a positive int")
    resumes = [
        other
        for other in spec.actions
        if other.kind == "readdir_resume" and other.payload["stream"] == action.id
    ]
    if len(resumes) != 1:
        raise SpecError(f"stream {action.id!r} needs exactly one readdir_resume")


def _validate_readdir_resume(spec: RunSpec, action: Action) -> None:
    """Require a known stream, on the same actor or ordered after its pause."""
    stream_id = action.payload["stream"]
    if not any(
        other.kind == "readdir_stream" and other.id == stream_id
        for other in spec.actions
    ):
        raise SpecError(
            f"readdir_resume {action.id!r} names unknown stream {stream_id!r}"
        )
    same_actor = any(
        stream_id in actor.actions
        and action.id in actor.actions
        and actor.actions.index(stream_id) < actor.actions.index(action.id)
        for actor in spec.actors
    )
    if not same_actor and (stream_id, "finished") not in action.requires:
        raise SpecError(
            f"readdir_resume {action.id!r} must follow {stream_id!r} on its actor "
            f"or require [{stream_id!r}, 'finished']"
        )


def _validate_fd_use(spec: RunSpec, action: Action) -> None:
    """Require ``fd`` to name an open finished before this action and not yet closed."""
    fd = action.payload["fd"]
    if not any(other.kind == "open" and other.id == fd for other in spec.actions):
        raise SpecError(
            f"{action.kind} {action.id!r} fd must name an open action, got {fd!r}"
        )
    earlier = finish_before(spec, action.id)
    if fd not in earlier:
        raise SpecError(
            f"{action.kind} {action.id!r} must follow open {fd!r} on its actor "
            f"or require [{fd!r}, 'finished']"
        )
    for other in spec.actions:
        if (
            other.kind == "close"
            and other.id != action.id
            and other.payload["fd"] == fd
            and other.id in earlier
        ):
            raise SpecError(
                f"{action.kind} {action.id!r} uses fd {fd!r} after close {other.id!r}"
            )


def _validate_read_fd(spec: RunSpec, action: Action) -> None:
    _validate_fd_use(spec, action)


def _validate_close(spec: RunSpec, action: Action) -> None:
    """One close per fd, ordered after every read_fd on that fd."""
    fd = action.payload["fd"]
    closes = [
        other.id
        for other in spec.actions
        if other.kind == "close" and other.payload["fd"] == fd
    ]
    if len(closes) != 1:
        raise SpecError(f"fd {fd!r} allows one close, got {closes}")
    earlier = finish_before(spec, action.id)
    for other in spec.actions:
        if (
            other.kind == "read_fd"
            and other.payload["fd"] == fd
            and other.id not in earlier
        ):
            raise SpecError(
                f"close {action.id!r} must follow read_fd {other.id!r} on fd {fd!r}"
            )
    _validate_fd_use(spec, action)


def _model_directories(spec: RunSpec, before: Action) -> set[str]:
    """Return directories the model knows when ``before`` starts.

    Modeled-file parents and the wide dir exist from setup; a ``mkdir`` path
    counts once the mkdir must finish before ``before`` starts.
    """
    known = {
        str(PurePosixPath(path).parent)
        for path in (*repository(spec)["registers"], *CHURN_FILES)
    }
    wide = wide_dir(spec)
    if wide is not None:
        known.add(wide["name"])
    earlier = finish_before(spec, before.id)
    known.update(
        a.payload["path"] for a in spec.actions if a.kind == "mkdir" and a.id in earlier
    )
    return known


def _validate_new_path(spec: RunSpec, action: Action, field: str) -> None:
    """Require a mount-relative path whose parent directory the model knows."""
    value = action.payload[field]
    _validate_mount_relative(value, f"{action.kind} {field}")
    if str(PurePosixPath(value).parent) not in _model_directories(spec, action):
        raise SpecError(f"{action.kind} {field} {value!r} has no parent in the model")


def _validate_move(spec: RunSpec, action: Action) -> None:
    """Require a register source and a destination in a known directory;
    an `sl mv` destination is never itself a directory (no `into` case).

    Modeled: the daemon serves the move on harness-authored
    state, so a failure outside every nemesis window is the daemon's.
    """
    source = action.payload["source"]
    if source not in repository(spec)["registers"]:
        raise SpecError(
            f"{action.kind} source {source!r} is not in repository.registers"
        )
    if action.payload["destination"] == source:
        raise SpecError(f"{action.kind} {action.id!r} moves {source!r} onto itself")
    _validate_new_path(spec, action, "destination")
    destination = action.payload["destination"]
    if action.kind != "sl_mv":
        return
    if (
        destination in _model_directories(spec, action)
        or any(
            other.kind == "mkdir" and other.payload["path"] == destination
            for other in spec.actions
        )
        or any(
            change["path"] == destination
            for change in repository(spec).get("type_changes", [])
        )
    ):
        raise SpecError(f"{action.kind} destination {destination!r} may be a directory")


def _validate_mkdir(spec: RunSpec, action: Action) -> None:
    """Require a new path in a known directory that is not a modeled file.

    Modeled: the daemon serves the mkdir on harness-authored
    state, so a failure outside every nemesis window is the daemon's.
    """
    if action.payload["path"] in (*repository(spec)["registers"], *CHURN_FILES):
        raise SpecError(f"mkdir path {action.payload['path']!r} is a modeled file")
    _validate_new_path(spec, action, "path")


def _validate_readdir(spec: RunSpec, action: Action) -> None:
    """Require a directory the model knows and a daemon channel.

    Modeled: the daemon serves a known directory, so a failure
    outside every nemesis window is the daemon's.
    """
    path = action.payload["path"]
    _validate_mount_relative(path, "readdir path")
    if path not in _model_directories(spec, action):
        raise SpecError(f"readdir path {path!r} is not a directory the model knows")
    channel = action.payload["channel"]
    if channel not in READDIR_CHANNELS:
        raise SpecError(f"readdir channel must be one of {sorted(READDIR_CHANNELS)}")


def _validate_unload_inodes(spec: RunSpec, action: Action) -> None:
    """Require a mount-relative subtree and a nonnegative unload age."""
    root = PurePosixPath(action.payload["path"])
    if not action.payload["path"] or root.is_absolute() or ".." in root.parts:
        raise SpecError("unload_inodes path must be mount-relative")
    age = action.payload.get("age_seconds", 0)
    if type(age) is not int or age < 0:
        raise SpecError("unload_inodes age_seconds must be a nonnegative int")


def _field(
    name: str,
    kind: FieldKind,
    *,
    required: bool = True,
    consumed: bool = True,
    schema: tuple[FieldSpec, ...] | None = None,
) -> FieldSpec:
    return FieldSpec(name, kind, required=required, consumed=consumed, schema=schema)


def _diagnostic(name: str, kind: FieldKind, *, required: bool = True) -> FieldSpec:
    return FieldSpec(name, kind, required=required, consumed=False)


_REASON = _diagnostic("reason", FieldKind.TEXT)
_ERRNO = _field("errno", FieldKind.INT)
_OPTIONAL_ERRNO = _field("errno", FieldKind.INT, required=False)
_DIGEST = (
    _field("bytes_prefix", FieldKind.HEX),
    _field("length", FieldKind.INT),
    _field("sha256", FieldKind.HEX),
)
_SYSCALL_FAIL = (_REASON, _ERRNO)
_PROCESS_FAIL = (
    _REASON,
    _field("returncode", FieldKind.INT),
    _diagnostic("stdout", FieldKind.TEXT),
    _diagnostic("stderr", FieldKind.TEXT),
)
_THRIFT_SOURCE = _diagnostic("source", FieldKind.TEXT, required=False)

# Daemon death (ECONNABORTED, ENOTCONN) and an injected fault error (EIO)
# need a nemesis; a request parked past the FUSE timeout fails ETIMEDOUT.
_NEMESIS_ERRNOS = {
    "ECONNABORTED": ErrnoUse.FAULT,
    "ENOTCONN": ErrnoUse.FAULT,
    "EIO": ErrnoUse.FAULT,
    "ETIMEDOUT": ErrnoUse.TIMEOUT,
}
# A `read_fd` or `close` whose open failed (or never ran) holds no
# descriptor and fails EBADF on both sides: legal exactly when the open
# was not acked (`contracts.checkout.state_observation`; `close` is unmodeled).
_DESCRIPTOR_ERRNOS = {
    "EBADF": ErrnoUse.STATE,
    "ECONNABORTED": ErrnoUse.FAULT,
    "ENOTCONN": ErrnoUse.FAULT,
    "ETIMEDOUT": ErrnoUse.TIMEOUT,
}
# A descriptor opened on a directory (FUSE OPENDIR) fails `read` EISDIR.
_DESCRIPTOR_READ_ERRNOS = {**_DESCRIPTOR_ERRNOS, "EISDIR": ErrnoUse.STATE}


def _state_errnos(*names: str) -> dict[str, ErrnoUse]:
    """Return the errnos of one syscall: state-dependent names plus the nemesis set."""
    return {**{name: ErrnoUse.STATE for name in names}, **_NEMESIS_ERRNOS}


# A failure without an errno is legal in some states: a Thrift EdenError
# carries its POSIX code in `error_code` (read) or, if any, `errno` (readdir),
# and a Sapling command reports only its exit status. The
# contracts hook decides each from the state, or leaves it not evaluated; an
# errno-less read failure without ENOENT or EISDIR is legal in no state.
_ERRNOLESS = {NO_ERRNO: ErrnoUse.STATE}
# A Thrift status names its EdenError type. `EdenMount::co_diff` refuses a
# status while a checkout is in progress or interrupted
# (`CHECKOUT_IN_PROGRESS`) and for a commit other than EdenFS's parent
# (`OUT_OF_DATE_PARENT`, as for Sapling's `.` after a Thrift checkout,
# measured by `reference_live_test`): both depend on state. Any other type
# (`GENERIC_ERROR`, `POSIX_ERROR`, ...) is undeclared, so `core.input-legality`
# judges it directly.
_THRIFT_STATUS_ERRORS = {
    "CHECKOUT_IN_PROGRESS": ErrnoUse.STATE,
    "OUT_OF_DATE_PARENT": ErrnoUse.STATE,
}


def _entries(
    name: str, members: tuple[FieldSpec, ...], *, required: bool = True
) -> FieldSpec:
    """Return a keyed collection field: a list of objects, one per key."""
    return _field(name, FieldKind.OBJECT_LIST, required=required, schema=members)


_PATH_KEY = _field("path", FieldKind.TEXT)

_RESULTS: Mapping[str, ResultSchema] = {
    "write": ResultSchema(
        ok=(_diagnostic("bytes_written", FieldKind.INT),),
        fail=(
            _REASON,
            _OPTIONAL_ERRNO,
            _field("close_errno", FieldKind.INT, required=False),
            _diagnostic("bytes_written", FieldKind.INT, required=False),
        ),
        errnos=_state_errnos("ENOENT", "EISDIR", "ENOTDIR"),
    ),
    "read": ResultSchema(
        ok=_DIGEST,
        fail=(
            _REASON,
            _OPTIONAL_ERRNO,
            _THRIFT_SOURCE,
            _diagnostic("error_type", FieldKind.TEXT, required=False),
            _field("error_code", FieldKind.INT, required=False),
            _diagnostic("message", FieldKind.TEXT, required=False),
        ),
        errnos={**_state_errnos("ENOENT", "EISDIR", "ENOTDIR"), **_ERRNOLESS},
    ),
    "read_fd": ResultSchema(
        ok=_DIGEST, fail=(_REASON, _OPTIONAL_ERRNO), errnos=_DESCRIPTOR_READ_ERRNOS
    ),
    "getattr": ResultSchema(
        ok=(
            _field("size", FieldKind.INT),
            _diagnostic("mtime", FieldKind.JSON),
            _diagnostic("mtime_history", FieldKind.JSON),
            _field("mtime_ns", FieldKind.INT, required=False),
            _field("mode", FieldKind.INT),
            _field("fuse_type", FieldKind.TEXT),
            _field("thrift_type", FieldKind.TEXT, required=False),
            _diagnostic("thrift_error", FieldKind.TEXT, required=False),
        ),
        fail=_SYSCALL_FAIL,
        errnos=_state_errnos("ENOENT", "ENOTDIR"),
    ),
    "open": ResultSchema(
        ok=(_diagnostic("fd", FieldKind.INT),),
        fail=_SYSCALL_FAIL,
        errnos=_state_errnos("ENOENT", "ENOTDIR"),
    ),
    "close": ResultSchema(
        ok=(_diagnostic("fd", FieldKind.INT),),
        fail=_SYSCALL_FAIL,
        errnos=_DESCRIPTOR_ERRNOS,
    ),
    "rename": ResultSchema(
        ok=(
            _diagnostic("source", FieldKind.TEXT),
            _diagnostic("destination", FieldKind.TEXT),
        ),
        fail=_SYSCALL_FAIL,
        errnos=_state_errnos("ENOENT", "EISDIR", "ENOTEMPTY", "EEXIST", "ENOTDIR"),
    ),
    "unlink": ResultSchema(
        ok=(_diagnostic("path", FieldKind.TEXT),),
        fail=_SYSCALL_FAIL,
        errnos=_state_errnos("ENOENT", "EISDIR", "ENOTDIR"),
    ),
    "mkdir": ResultSchema(
        ok=(_diagnostic("path", FieldKind.TEXT),),
        fail=_SYSCALL_FAIL,
        errnos=_state_errnos("EEXIST", "ENOENT", "ENOTDIR"),
    ),
    "chmod": ResultSchema(
        ok=(_diagnostic("path", FieldKind.TEXT), _diagnostic("mode", FieldKind.INT)),
        fail=_SYSCALL_FAIL,
        errnos=_state_errnos("ENOENT", "ENOTDIR"),
    ),
    "readdir": ResultSchema(
        ok=(
            _diagnostic("channel", FieldKind.TEXT),
            _field("entries", FieldKind.TEXT_LIST),
        ),
        fail=(
            _REASON,
            _OPTIONAL_ERRNO,
            _THRIFT_SOURCE,
            _field("error_type", FieldKind.TEXT, required=False),
        ),
        errnos={**_state_errnos("ENOENT", "ENOTDIR", "EINVAL"), **_ERRNOLESS},
    ),
    "readdir_stream": ResultSchema(
        ok=(
            _diagnostic("pgid", FieldKind.INT),
            _diagnostic("paused_at", FieldKind.INT),
            _diagnostic("pos_at_pause", FieldKind.INT),
        ),
        fail=(_REASON, _diagnostic("stderr", FieldKind.TEXT)),
    ),
    "readdir_resume": ResultSchema(
        ok=(
            _diagnostic("stream", FieldKind.TEXT),
            _field("entries", FieldKind.TEXT_LIST),
            _diagnostic("pos_at_pause", FieldKind.INT),
            _diagnostic("pos_at_end", FieldKind.INT),
        ),
        fail=(_REASON, _diagnostic("stderr", FieldKind.TEXT)),
    ),
    "status": ResultSchema(
        ok=(
            _entries("status", (_PATH_KEY, _field("code", FieldKind.TEXT))),
            _field("diff_paths", FieldKind.TEXT_LIST, required=False),
            _diagnostic("parent", FieldKind.TEXT, required=False),
        ),
        fail=(
            _REASON,
            _field("returncode", FieldKind.INT, required=False),
            _diagnostic("stdout", FieldKind.TEXT, required=False),
            _field("stderr", FieldKind.TEXT, required=False),
            _THRIFT_SOURCE,
            _field("error_type", FieldKind.TEXT, required=False),
        ),
        errnos=_ERRNOLESS,
        error_types=_THRIFT_STATUS_ERRORS,
    ),
    "checkout": ResultSchema(
        ok=(
            _diagnostic("revision", FieldKind.INT),
            _diagnostic("node", FieldKind.TEXT),
            _diagnostic("via", FieldKind.TEXT),
            _diagnostic("parent", FieldKind.TEXT, required=False),
            _field("conflicts", FieldKind.INT, required=False),
            FieldSpec(
                "conflict_types",
                FieldKind.OBJECT_LIST,
                required=False,
                consumed=False,
                schema=(_PATH_KEY, _field("type", FieldKind.TEXT)),
            ),
        ),
        fail=(
            _REASON,
            _field("returncode", FieldKind.INT, required=False),
            _diagnostic("stdout", FieldKind.TEXT, required=False),
            _field("stderr", FieldKind.TEXT, required=False),
            _THRIFT_SOURCE,
            _field("error_type", FieldKind.TEXT, required=False),
        ),
    ),
    "sl_rm": ResultSchema(
        ok=(_diagnostic("path", FieldKind.TEXT),),
        fail=_PROCESS_FAIL,
        errnos=_ERRNOLESS,
    ),
    "sl_mv": ResultSchema(
        ok=(
            _diagnostic("source", FieldKind.TEXT),
            _diagnostic("destination", FieldKind.TEXT),
        ),
        fail=_PROCESS_FAIL,
        errnos=_ERRNOLESS,
    ),
    "manifest_presence": ResultSchema(
        ok=tuple(
            _entries(name, (_PATH_KEY, _field("present", FieldKind.BOOL)))
            for name in ("deleted_presence", "added_presence")
        ),
    ),
    "journal_window": ResultSchema(
        ok=(
            _field("mount_generation", FieldKind.INT),
            # The cursor the window ends at; completeness is decided from
            # `since_sequence`, truncation, and lost changes, so no rule reads it.
            _diagnostic("sequence_number", FieldKind.INT),
            _diagnostic("since_sequence", FieldKind.INT, required=False),
            _entries(
                "changed_paths",
                (
                    _PATH_KEY,
                    _field("existed_before", FieldKind.BOOL),
                    _field("existed_after", FieldKind.BOOL),
                ),
                required=False,
            ),
            _field("truncated", FieldKind.BOOL, required=False),
            _field("lost_changes", FieldKind.TEXT_LIST, required=False),
        ),
        fail=(_REASON, _THRIFT_SOURCE),
    ),
    "status_counts": ResultSchema(
        ok=(
            FieldSpec(
                "counts",
                FieldKind.OBJECT_LIST,
                required=False,
                consumed=False,
                schema=(_field("code", FieldKind.TEXT), _field("count", FieldKind.INT)),
            ),
            _diagnostic("sample", FieldKind.TEXT_LIST, required=False),
            _diagnostic("refused", FieldKind.TEXT, required=False),
        ),
    ),
    "background_read": ResultSchema(
        ok=(
            _diagnostic("pgid", FieldKind.INT),
            _diagnostic("returncode", FieldKind.INT),
            _diagnostic("stdout", FieldKind.TEXT),
            _diagnostic("stderr", FieldKind.TEXT),
            _diagnostic("sweeps", FieldKind.INT),
        ),
        fail=(
            _diagnostic("pgid", FieldKind.INT),
            _diagnostic("returncode", FieldKind.INT),
            _diagnostic("stdout", FieldKind.TEXT),
            _diagnostic("stderr", FieldKind.TEXT),
            _diagnostic("sweeps", FieldKind.INT),
        ),
        info=(_diagnostic("pgid", FieldKind.INT, required=False),),
    ),
    "unload_inodes": ResultSchema(
        ok=(
            _diagnostic("path", FieldKind.TEXT),
            _diagnostic("prepared", FieldKind.INT),
            _diagnostic("num_invalidated", FieldKind.INT),
        ),
    ),
}

# Kinds a differential run may share; their `ok` and `fail` results declare
# the reference side, which `HistoryFacts.build` decodes and whose presence
# it checks against `expects_reference`.
PAIRED_KINDS = frozenset(
    {
        "write",
        "read",
        "read_fd",
        "getattr",
        "open",
        "close",
        "rename",
        "unlink",
        "mkdir",
        "chmod",
        "readdir",
        "status",
        "checkout",
        "sl_rm",
        "sl_mv",
    }
)
RESULTS: Mapping[str, ResultSchema] = {
    kind: paired(schema) if kind in PAIRED_KINDS else schema
    for kind, schema in _RESULTS.items()
}

# The exact field sets each channel records, where a kind's schema above
# must accept the union: "a b?" is a variant with `b` optional. A schema
# cannot say "exactly these per channel"; the result-shape property judges recorded results against these variants.
CHANNEL_SHAPES: Mapping[tuple[str, str], Mapping[str, tuple[str, ...]]] = {
    ("getattr", "ok"): {
        "fuse": (
            "size mtime mtime_history mtime_ns? mode fuse_type thrift_type",
            "size mtime mtime_history mtime_ns? mode fuse_type thrift_error",
        ),
    },
    ("read", "fail"): {
        "fuse": ("reason errno", "reason"),
        "thrift": ("reason source error_type error_code? message", "reason"),
    },
    ("readdir", "fail"): {
        "fuse": ("reason errno",),
        "thrift": ("reason source error_type errno?", "reason source", "reason"),
    },
    ("status", "ok"): {
        "sl": ("status diff_paths parent?",),
        "thrift": ("status parent",),
    },
    ("status", "fail"): {
        "sl": ("reason", "reason returncode stdout stderr"),
        "thrift": ("reason source error_type",),
    },
    ("checkout", "ok"): {
        "sl": ("revision node via parent?",),
        "thrift": ("revision node via conflicts conflict_types?",),
    },
    ("checkout", "fail"): {
        "sl": ("reason returncode stdout stderr",),
        "thrift": ("reason source error_type?",),
    },
}


def result_channel(action: Action) -> str:
    """Return the channel whose result shape an action records."""
    if action.kind == "checkout":
        return "thrift" if action.payload["via"] == "thrift" else "sl"
    return action.payload.get("channel", "fuse")


def channel_shape_problem(
    action: Action, outcome: str, fields: Mapping[str, Any]
) -> str | None:
    """Return why a decoded result does not have its channel's exact fields, or None.

    The reference side a differential result carries is not part of the
    shape. Kinds and outcomes without per-channel variants have none.
    """
    variants = CHANNEL_SHAPES.get((action.kind, outcome))
    if variants is None:
        return None
    channel = result_channel(action)
    keys = set(fields) - {REFERENCE_RESULT}
    for variant in variants.get(channel, ()):
        names = variant.split()
        required = {name for name in names if not name.endswith("?")}
        optional = {name[:-1] for name in names if name.endswith("?")}
        if required <= keys <= required | optional:
            return None
    return (
        f"{action.kind} {outcome} over {channel} recorded {sorted(keys)}, "
        f"not one of {list(variants.get(channel, ()))}"
    )


def _open_path(spec: RunSpec, fd: str) -> str:
    return spec.action(fd).payload["path"]


def _stream_path(spec: RunSpec, stream: str) -> str:
    return spec.action(stream).payload["path"]


def _channels(*names: str) -> frozenset[str]:
    return frozenset(names)


def _footprint_write(_spec: RunSpec, action: Action) -> Footprint:
    return Footprint(
        writes=frozenset({action.payload["path"]}), channels=_channels("fuse")
    )


def _footprint_move(_spec: RunSpec, action: Action) -> Footprint:
    """A move may carry a type-changed directory, so it writes both subtrees."""
    moved = frozenset({action.payload["source"], action.payload["destination"]})
    if action.kind == "rename":
        return Footprint(subtrees_written=moved, channels=_channels("fuse"))
    return Footprint(
        subtrees_written=moved,
        subtrees_read=frozenset({ROOT}),
        channels=_channels("sl"),
    )


def _footprint_sl_rm(_spec: RunSpec, action: Action) -> Footprint:
    """`sl rm` of a type-changed directory removes every file under it."""
    return Footprint(
        subtrees_written=frozenset({action.payload["path"]}),
        subtrees_read=frozenset({ROOT}),
        channels=_channels("sl"),
    )


def _footprint_read(_spec: RunSpec, action: Action) -> Footprint:
    channel = action.payload.get("channel", "fuse")
    return Footprint(
        reads=frozenset({action.payload["path"]}), channels=_channels(channel)
    )


def _footprint_getattr(_spec: RunSpec, action: Action) -> Footprint:
    return Footprint(
        reads=frozenset({action.payload["path"]}), channels=_channels("fuse", "thrift")
    )


def _under_or_at(path: str, root: str) -> bool:
    return path == root or path.startswith(root + "/")


def descriptor_paths(spec: RunSpec, path: str) -> frozenset[str]:
    """Return the subtree roots an object opened at `path` can be moved under.

    Every path the object can be bound to during the run is `path` itself
    or lies under one of the returned roots: a move of the object's path
    or of an ancestor directory, either way (the same reach the contracts
    gives a held descriptor, `contracts.checkout.descriptor_plans`), carries it to its
    destination plus the rest of its path. A move applies when its source
    is at or above `path`, or at, above or under a root already found, and
    then adds its destination as a root. The closure is over the run's move
    endpoints, so it is at most their number, whatever the moves' nesting.
    A move into its own subtree carries nothing (the kernel refuses it).
    """
    moves = [
        (
            normalize_path(action.payload["source"]),
            normalize_path(action.payload["destination"]),
        )
        for action in spec.actions
        if action.kind in ("rename", "sl_mv")
    ]
    moves = [
        (source, destination)
        for source, destination in moves
        if not _under_or_at(destination, source)
        and not _under_or_at(source, destination)
    ]
    opened = normalize_path(path)
    roots: set[str] = set()
    grown = True
    while grown:
        grown = False
        for source, destination in moves:
            for old, new in ((source, destination), (destination, source)):
                if new not in roots and (
                    _under_or_at(opened, old)
                    or any(
                        _under_or_at(root, old) or _under_or_at(old, root)
                        for root in roots
                    )
                ):
                    roots.add(new)
                    grown = True
    return frozenset(roots)


def _footprint_descriptor(spec: RunSpec, action: Action) -> Footprint:
    """A descriptor reads the object its open bound, wherever moves can carry it."""
    opened = _open_path(spec, action.payload["fd"])
    return Footprint(
        reads=frozenset({opened}),
        subtrees_read=descriptor_paths(spec, opened),
        channels=_channels("fuse"),
    )


def _footprint_listing(spec: RunSpec, action: Action) -> Footprint:
    if action.kind == "readdir_resume":
        path = _stream_path(spec, action.payload["stream"])
        channel = "fuse"
    else:
        path = action.payload["path"]
        channel = action.payload.get("channel", "fuse")
    return Footprint(subtrees_read=frozenset({path}), channels=_channels(channel))


def _footprint_checkout(_spec: RunSpec, action: Action) -> Footprint:
    channel = "thrift" if action.payload["via"] == "thrift" else "sl"
    return Footprint(subtrees_written=frozenset({ROOT}), channels=_channels(channel))


def _footprint_whole_read(channel: str) -> Callable[[RunSpec, Action], Footprint]:
    def footprint(_spec: RunSpec, action: Action) -> Footprint:
        """Return a whole-checkout read through the action's channel."""
        return Footprint(
            subtrees_read=frozenset({ROOT}),
            channels=_channels(action.payload.get("channel", channel)),
        )

    return footprint


def _no_footprint(_spec: RunSpec, _action: Action) -> Footprint:
    return Footprint()


@dataclasses.dataclass(frozen=True)
class _Effects:
    """The static effect declaration of one kind."""

    footprint: Callable[[RunSpec, Action], Footprint]
    roles: frozenset[Role]
    binding: Binding
    grouping: Grouping


def _effects(
    footprint: Callable[[RunSpec, Action], Footprint],
    roles: tuple[Role, ...],
    binding: Binding,
    grouping: Grouping,
) -> _Effects:
    return _Effects(footprint, frozenset(roles), binding, grouping)


_OBSERVE = (Role.OBSERVE,)
_NONE = _effects(_no_footprint, (), Binding.NONE, Grouping.NONE)
EFFECTS: Mapping[str, _Effects] = {
    "write": _effects(
        _footprint_write,
        (Role.PRODUCE, Role.CAN_SUPERSEDE),
        Binding.PATH,
        Grouping.SINGLE,
    ),
    "rename": _effects(
        _footprint_move,
        (Role.TRANSFER, Role.REMOVE, Role.CAN_SUPERSEDE),
        Binding.PATH,
        Grouping.ATOMIC,
    ),
    "unlink": _effects(
        _footprint_write,
        (Role.REMOVE, Role.CAN_SUPERSEDE),
        Binding.PATH,
        Grouping.ATOMIC,
    ),
    "mkdir": _effects(_footprint_write, (Role.PRODUCE,), Binding.PATH, Grouping.ATOMIC),
    "chmod": _effects(
        _footprint_write,
        (Role.PRODUCE, Role.CAN_SUPERSEDE),
        Binding.PATH,
        Grouping.SINGLE,
    ),
    "sl_rm": _effects(
        _footprint_sl_rm,
        (Role.REMOVE, Role.PRODUCE, Role.CAN_SUPERSEDE),
        Binding.PATH,
        Grouping.ATOMIC,
    ),
    "sl_mv": _effects(
        _footprint_move,
        (Role.TRANSFER, Role.PRODUCE, Role.REMOVE, Role.CAN_SUPERSEDE),
        Binding.PATH,
        Grouping.ATOMIC,
    ),
    "checkout": _effects(
        _footprint_checkout,
        (Role.PRODUCE, Role.REMOVE, Role.CAN_SUPERSEDE),
        Binding.AGGREGATE,
        Grouping.PROGRESSIVE,
    ),
    "read": _effects(_footprint_read, _OBSERVE, Binding.PATH, Grouping.NONE),
    "getattr": _effects(_footprint_getattr, _OBSERVE, Binding.PATH, Grouping.NONE),
    "open": _effects(_footprint_read, _OBSERVE, Binding.DESCRIPTOR, Grouping.NONE),
    "read_fd": _effects(
        _footprint_descriptor, _OBSERVE, Binding.DESCRIPTOR, Grouping.NONE
    ),
    "close": _effects(_footprint_descriptor, (), Binding.DESCRIPTOR, Grouping.NONE),
    "readdir": _effects(_footprint_listing, _OBSERVE, Binding.AGGREGATE, Grouping.NONE),
    "readdir_stream": _effects(
        _footprint_listing, _OBSERVE, Binding.AGGREGATE, Grouping.NONE
    ),
    "readdir_resume": _effects(
        _footprint_listing, _OBSERVE, Binding.AGGREGATE, Grouping.NONE
    ),
    "status": _effects(
        _footprint_whole_read("sl"), _OBSERVE, Binding.AGGREGATE, Grouping.NONE
    ),
    "manifest_presence": _effects(
        _footprint_whole_read("fuse"), _OBSERVE, Binding.AGGREGATE, Grouping.NONE
    ),
    "journal_window": _effects(
        _footprint_whole_read("thrift"), _OBSERVE, Binding.AGGREGATE, Grouping.NONE
    ),
    "status_counts": _effects(
        _footprint_whole_read("sl"), _OBSERVE, Binding.AGGREGATE, Grouping.NONE
    ),
    "background_read": _NONE,
    "unload_inodes": _NONE,
}


def _subject_field(name: str) -> Callable[[RunSpec, Action], str]:
    def subject(_spec: RunSpec, action: Action) -> str:
        """Return the canonical payload path an input's failure is about."""
        return normalize_path(action.payload[name])

    return subject


def _descriptor_subject(spec: RunSpec, action: Action) -> str:
    """Return the path a descriptor input's open named."""
    return normalize_path(_open_path(spec, action.payload["fd"]))


# The path each modeled input whose footprint does not name exactly its
# subject is about: a move's source, a descriptor's opened path, and the
# path `sl rm` removes (its footprint also reads the whole checkout).
INPUT_SUBJECTS: Mapping[str, Callable[[RunSpec, Action], str]] = {
    "rename": _subject_field("source"),
    "sl_mv": _subject_field("source"),
    "sl_rm": _subject_field("path"),
    "open": _subject_field("path"),
    "read_fd": _descriptor_subject,
    "close": _descriptor_subject,
}


def _definition(
    kind: str,
    axis: str,
    fields: frozenset[str],
    required: frozenset[str],
    types: Mapping[str, type],
    validate: Callable[[RunSpec, Action], None],
    *,
    modeled: bool = False,
    spawns_process: bool = False,
    sample: Callable[
        [random.Random, Mapping[str, Any]], Mapping[str, Any] | None
    ] = _sample_none,
) -> ActionDef:
    effects = EFFECTS[kind]
    return ActionDef(
        kind,
        axis,
        fields,
        required,
        types,
        validate,
        modeled,
        spawns_process,
        sample,
        result=RESULTS[kind],
        footprint=effects.footprint,
        roles=effects.roles,
        binding=effects.binding,
        grouping=effects.grouping,
        input_subject=INPUT_SUBJECTS.get(kind),
    )


EDENFS_ACTION_DEFS = {
    "background_read": _definition(
        "background_read",
        "nemesis",
        frozenset({"root", "seconds"}),
        frozenset({"root", "seconds"}),
        {"root": str},
        _validate_background_read,
        spawns_process=True,
        sample=_sample_background_read,
    ),
    "unload_inodes": _definition(
        "unload_inodes",
        "nemesis",
        frozenset({"path", "age_seconds"}),
        frozenset({"path"}),
        {"path": str, "age_seconds": int},
        _validate_unload_inodes,
    ),
    "write": _definition(
        "write",
        "input",
        frozenset({"path", "value_id"}),
        frozenset({"path", "value_id"}),
        {"path": str, "value_id": str},
        _validate_write,
        modeled=True,
        sample=write_sampler(_sample_path),
    ),
    "read": _definition(
        "read",
        "input",
        frozenset({"path", "channel", "sync_timeout_seconds"}),
        frozenset({"path", "channel"}),
        {"path": str, "channel": str, "sync_timeout_seconds": int},
        _validate_read,
        modeled=True,
        sample=_sample_read,
    ),
    "getattr": _definition(
        "getattr",
        "input",
        frozenset({"path"}),
        frozenset({"path"}),
        {"path": str},
        _validate_getattr,
        modeled=True,
        sample=_sample_getattr,
    ),
    "status": _definition(
        "status",
        "input",
        frozenset({"channel", "revision"}),
        frozenset({"channel"}),
        {"channel": str, "revision": int},
        _validate_status,
        modeled=True,
        spawns_process=True,
        sample=_sample_status,
    ),
    "checkout": _definition(
        "checkout",
        "input",
        frozenset({"revision", "via", "mode"}),
        frozenset({"revision", "via", "mode"}),
        {"revision": int, "via": str, "mode": str},
        _validate_checkout,
        spawns_process=True,
        sample=_sample_checkout,
    ),
    "readdir_stream": _definition(
        "readdir_stream",
        "input",
        frozenset({"path", "pause_after"}),
        frozenset({"path", "pause_after"}),
        {"path": str, "pause_after": int},
        _validate_readdir_stream,
        modeled=True,
        spawns_process=True,
    ),
    "readdir": _definition(
        "readdir",
        "input",
        frozenset({"path", "channel"}),
        frozenset({"path", "channel"}),
        {"path": str, "channel": str},
        _validate_readdir,
        modeled=True,
        sample=_sample_readdir,
    ),
    "readdir_resume": _definition(
        "readdir_resume",
        "input",
        frozenset({"stream"}),
        frozenset({"stream"}),
        {"stream": str},
        _validate_readdir_resume,
        modeled=True,
    ),
    "open": _definition(
        "open",
        "input",
        frozenset({"path"}),
        frozenset({"path"}),
        {"path": str},
        _validate_path,
        modeled=True,
        sample=_sample_open,
    ),
    "read_fd": _definition(
        "read_fd",
        "input",
        frozenset({"fd"}),
        frozenset({"fd"}),
        {"fd": str},
        _validate_read_fd,
        modeled=True,
    ),
    "close": _definition(
        "close",
        "input",
        frozenset({"fd"}),
        frozenset({"fd"}),
        {"fd": str},
        _validate_close,
    ),
    "rename": _definition(
        "rename",
        "input",
        frozenset({"source", "destination"}),
        frozenset({"source", "destination"}),
        {"source": str, "destination": str},
        _validate_move,
        modeled=True,
        sample=_move_sampler("rename"),
    ),
    "unlink": _definition(
        "unlink",
        "input",
        frozenset({"path"}),
        frozenset({"path"}),
        {"path": str},
        _validate_path,
        modeled=True,
        sample=_remove_sampler("unlink"),
    ),
    "mkdir": _definition(
        "mkdir",
        "input",
        frozenset({"path"}),
        frozenset({"path"}),
        {"path": str},
        _validate_mkdir,
        modeled=True,
        sample=_sample_mkdir,
    ),
    "chmod": _definition(
        "chmod",
        "input",
        frozenset({"path", "mode"}),
        frozenset({"path", "mode"}),
        {"path": str, "mode": int},
        _validate_chmod,
        modeled=True,
        sample=_sample_chmod,
    ),
    "sl_rm": _definition(
        "sl_rm",
        "input",
        frozenset({"path"}),
        frozenset({"path"}),
        {"path": str},
        _validate_path,
        modeled=True,
        spawns_process=True,
        sample=_remove_sampler("sl_rm"),
    ),
    "sl_mv": _definition(
        "sl_mv",
        "input",
        frozenset({"source", "destination"}),
        frozenset({"source", "destination"}),
        {"source": str, "destination": str},
        _validate_move,
        modeled=True,
        spawns_process=True,
        sample=_move_sampler("sl_mv"),
    ),
    "manifest_presence": _definition(
        "manifest_presence",
        "observation",
        frozenset({"from_revision", "to_revision"}),
        frozenset({"from_revision", "to_revision"}),
        {"from_revision": int, "to_revision": int},
        _validate_manifest_presence,
        sample=lambda _rng, context: {
            "from_revision": 0,
            "to_revision": context["environment"]["repository"]["commit_count"] - 1,
        },
    ),
    "journal_window": _definition(
        "journal_window",
        "observation",
        frozenset({"since_sequence"}),
        frozenset(),
        {"since_sequence": int},
        _validate_environment_only,
        sample=lambda _rng, _context: {"since_sequence": 0},
    ),
    "status_counts": _definition(
        "status_counts",
        "observation",
        frozenset(),
        frozenset(),
        {},
        _validate_environment_only,
        sample=lambda _rng, _context: {},
    ),
}


# Differential tables. The reference side is a plain Linux filesystem holding
# a Sapling checkout of the same fixture; a shared action runs once on each.
EDEN_ONLY_NEMESIS = "eden-only-nemesis"
EDEN_ONLY_CHANNEL = "eden-only-channel"
EDEN_ONLY_OBSERVATION = "eden-only-observation"
EDEN_ONLY_HELD_INPUT = "eden-only-held-input"
HARNESS_ONLY = "harness-only"
ROOT_LISTING = "root-listing"

_SYSCALL_KINDS = frozenset(
    {
        "write",
        "read",
        "getattr",
        "open",
        "read_fd",
        "close",
        "rename",
        "unlink",
        "mkdir",
        "chmod",
        "readdir",
    }
)
_EDEN_ONLY_CHANNELS: Mapping[str, tuple[str, frozenset[str]]] = {
    "read": ("channel", frozenset({"thrift"})),
    "readdir": ("channel", frozenset({"thrift"})),
    "status": ("channel", frozenset({"thrift"})),
    "checkout": ("via", frozenset({"thrift"})),
}
_EDEN_ONLY_KINDS: Mapping[str, str] = {
    "background_read": EDEN_ONLY_NEMESIS,
    "unload_inodes": EDEN_ONLY_NEMESIS,
    "fault_install": EDEN_ONLY_NEMESIS,
    "fault_await": EDEN_ONLY_NEMESIS,
    "fault_remove": EDEN_ONLY_NEMESIS,
    "kill_process": EDEN_ONLY_NEMESIS,
    "blocked_faults": EDEN_ONLY_OBSERVATION,
    "manifest_presence": EDEN_ONLY_OBSERVATION,
    "journal_window": EDEN_ONLY_OBSERVATION,
    "status_counts": EDEN_ONLY_OBSERVATION,
    "readdir_stream": EDEN_ONLY_HELD_INPUT,
    "readdir_resume": EDEN_ONLY_HELD_INPUT,
    "sleep": HARNESS_ONLY,
}

_DIGEST_KEYS = ("bytes_prefix", "length", "sha256")
DIFFERENTIAL_OBSERVATIONS: Mapping[str, ObservationDef] = {
    "write": ObservationDef(),
    "read": ObservationDef(_DIGEST_KEYS),
    "read_fd": ObservationDef(_DIGEST_KEYS),
    "getattr": ObservationDef(
        ("size", "mode", "fuse_type"),
        conditional={"size": ("fuse_type", frozenset({"file"}))},
    ),
    "open": ObservationDef(),
    "close": ObservationDef(),
    "rename": ObservationDef(),
    "unlink": ObservationDef(),
    "mkdir": ObservationDef(),
    "chmod": ObservationDef(),
    "readdir": ObservationDef(("entries",), frozenset({"entries"})),
    "status": ObservationDef(
        ("status", "diff_paths"), frozenset({"status", "diff_paths"})
    ),
    "checkout": ObservationDef(),
    "sl_rm": ObservationDef(),
    "sl_mv": ObservationDef(),
}

TIMEOUT_CAPABLE_KINDS = _SYSCALL_KINDS


def differential_applicability(_spec: RunSpec, action: Action) -> Applicability:
    """Return whether an action runs on the reference checkout too.

    Faults, kills, inode unloads, held readdir streams, daemon-only
    observations, harness sleeps, and Thrift channels exist only on
    EdenFS, and the checkout root lists harness metadata only the
    reference has. Each carries a stable
    not-applicable reason.
    """
    reason = _EDEN_ONLY_KINDS.get(action.kind)
    if reason is not None:
        return not_applicable(reason)
    if action.kind not in DIFFERENTIAL_OBSERVATIONS:
        raise ValueError(f"no differential applicability for {action.kind!r}")
    channel = _EDEN_ONLY_CHANNELS.get(action.kind)
    if channel is not None and action.payload.get(channel[0]) in channel[1]:
        return not_applicable(EDEN_ONLY_CHANNEL)
    if action.kind == "readdir" and normalize_path(action.payload["path"]) == ROOT:
        return not_applicable(ROOT_LISTING)
    return SHARED


def expects_reference(spec: RunSpec, action: Action) -> bool:
    """Return whether an action owes a reference side in its terminal event.

    The one predicate the runtime that runs the reference side and the
    checker that reads it share: the run is differential and the action is
    shared. Validation admits only shared actions into a differential run.
    """
    return reference_clone(spec) and differential_applicability(spec, action).shared


def differential_footprint(spec: RunSpec, action: Action) -> Footprint:
    """Return the modeled paths an action reads and writes, as its ActionDef declares."""
    return ACTION_DEFS[action.kind].footprint(spec, action)


def timeout_capable(_spec: RunSpec, _facts: HistoryFacts, action: Action) -> bool:
    """Return whether an action's own reply crosses the FUSE channel.

    `FuseChannel` returns ETIMEDOUT for a request parked past its configured
    request timeout (fuse/FuseChannel.cpp:2466-2472). Thrift calls and
    Sapling processes report their own errors and are never
    timeout-capable. A pure function of the kind and channel.
    """
    if action.kind not in TIMEOUT_CAPABLE_KINDS:
        return False
    return action.payload.get("channel", "fuse") == "fuse"


def _validate_write_values(spec: RunSpec) -> None:
    """Require unique, encodable write values outside the fixture image namespace.

    A write's bytes name its writer (owning actor) and value ID, so unique
    value IDs make every write's bytes unique, and the reserved `init` and
    `commit*` names keep them distinct from every committed image.
    """
    seen: dict[str, str] = {}
    for action in spec.actions:
        if action.kind != "write":
            continue
        value_id, writer = action.payload["value_id"], spec.owner(action.id)
        if value_id in seen:
            raise SpecError(
                f"write {action.id!r} repeats value_id {value_id!r} of {seen[value_id]!r}"
            )
        seen[value_id] = action.id
        for name, what in ((value_id, "value_id"), (writer, "writer")):
            if is_reserved_writer(name):
                raise SpecError(
                    f"write {action.id!r} {what} {name!r} is reserved for fixture images"
                )
        try:
            encode_value(spec.seed, writer, value_id)
        except ValueError as error:
            raise SpecError(f"write {action.id!r}: {error}") from error


def _validate_fixture_images(spec: RunSpec) -> None:
    """Require the seed to fit every fixture image the run's registers and churn hold.

    A committed register image names the revision and a fixed-size path
    hash, so the newest revision bounds them all; a churn image names its
    path.
    """
    repo = repository(spec)
    try:
        for register in repo["registers"]:
            encode_committed(spec.seed, register, repo["commit_count"] - 1)
        for path in CHURN_FILES:
            initial_content(spec.seed, path)
    except ValueError as error:
        raise SpecError(
            f"seed {spec.seed} does not fit the fixture images: {error}"
        ) from error


def validate_run(spec: RunSpec) -> None:
    """Validate what no single action owns: the environment, fixture images, write values."""
    _validate_environment(spec)
    _validate_fixture_images(spec)
    _validate_write_values(spec)


def validate_comparable(spec: RunSpec) -> None:
    """Admit a differential run only as shared actions in a comparable schedule.

    Every two actions with conflicting footprints must be ordered by actor
    order or a finished `requires` edge. A non-differential run passes.
    """
    if reference_clone(spec):
        validate_differential(spec, differential_applicability, differential_footprint)


# A checkout overlapping an unlink in a directory its walk visits deadlocks
# the mount (T293953113: `TreeInode::removeImpl` retries while still holding
# the directory's rename lock and blocks on it, so the unlink never returns
# and the checkout's invalidations wait behind it); measured for FORCE and
# NORMAL. Generation never composes a checkout overlapping a remove or
# transfer on a path in its walk. A mkdir in a directory the parked walk
# visits waits for the checkout and completes once it is released (measured
# for FORCE and NORMAL), so a mkdir is not ordered. Lifting this ordering
# exposes the T294583260 checker gap: interleaved acked checkouts are accepted.
_WALK_BLOCKING_ROLES = frozenset({Role.REMOVE, Role.TRANSFER})


def _blocks_walk(kind: str) -> bool:
    """Return whether an action of `kind` may not overlap a forced checkout's walk."""
    definition = EDENFS_ACTION_DEFS.get(kind)
    return definition is not None and bool(definition.roles & _WALK_BLOCKING_ROLES)


# The RunSpec metadata entry a reproducer deck declares to run a checkout walk
# concurrent with a remove or transfer: `{"task": "T293953113"}`.
WALK_DEADLOCK_TASK = "T293953113"


def validate_forced_checkout_walks(spec: RunSpec) -> None:
    """Reject a checkout that may overlap a pair `forbid_overlap` names.

    Generation orders such pairs; this keeps hand-written specs, replays,
    and shrunk specs from reintroducing that deadlock (T293953113). A
    spec whose metadata names that task reproduces it and is exempt.
    """
    if spec.metadata.get("task") == WALK_DEADLOCK_TASK:
        return
    checkouts = any(action.kind == "checkout" for action in spec.actions)
    blocking = any(_blocks_walk(action.kind) for action in spec.actions)
    if not (checkouts and blocking):
        return
    for first, second in may_overlap(spec):
        if forbid_overlap(spec, spec.action(first), spec.action(second)):
            error = SpecError(
                f"checkout and {first!r}/{second!r} may overlap: a checkout "
                "walk concurrent with a remove or transfer of a path it "
                "changes deadlocks the mount (T293953113; a mkdir is "
                "unmeasured and ordered too)"
            )
            error.waiting_on = WALK_DEADLOCK_TASK
            raise error


def forbid_overlap(spec: RunSpec, first: Action, second: Action) -> bool:
    """Return whether generation must order `first` and `second`.

    True for a checkout, in any mode, and a path-bound action whose
    declared roles remove or transfer a path its footprint shares with the
    checkout's walk, or a mkdir there. Another checkout is left to the
    checkout-in-progress guard.
    """
    for checkout, other in ((first, second), (second, first)):
        if checkout.kind != "checkout":
            continue
        definition = EDENFS_ACTION_DEFS.get(other.kind)
        if (
            definition is None
            or definition.binding != Binding.PATH
            or not _blocks_walk(other.kind)
        ):
            continue
        walk = EDENFS_ACTION_DEFS["checkout"].footprint(spec, checkout)
        if footprints_interact(walk, definition.footprint(spec, other)):
            return True
    return False


def produced_value(spec: RunSpec, action: Action | None, path: str) -> object:
    """Return what a producer leaves at a register, for the generation summary.

    A register holds a revision-specific image, so the initial state and
    every checkout produce `("revision", r)`; a write produces its own
    value. Anything else, and any other path, is unknown (None).
    """
    if path not in repository(spec)["registers"]:
        return None
    if action is None:
        return ("revision", repository(spec)["commit_count"] - 1)
    if action.kind == "checkout":
        return ("revision", action.payload["revision"])
    if action.kind == "write":
        return ("write", action.id)
    return None


def _with_run_validation(definition: ActionDef) -> ActionDef:
    """Return the definition whose validator also runs the whole-run checks, once.

    Validation visits actions in spec order: the first action's validator
    checks the environment before any payload check reads it, and the last
    one checks differential admission and complete-observation event sizes
    after every payload is valid.
    """
    payload_validate = definition.validate

    def validate(spec: RunSpec, action: Action) -> None:
        """Validate the payload, with the run checks on the first and last action."""
        if action.id == spec.actions[0].id:
            validate_run(spec)
        payload_validate(spec, action)
        if action.id == spec.actions[-1].id:
            validate_comparable(spec)
            validate_observation_sizes(spec)
            validate_forced_checkout_walks(spec)

    return dataclasses.replace(definition, validate=validate)


ACTION_DEFS: Mapping[str, ActionDef] = {
    kind: _with_run_validation(definition)
    for kind, definition in compose_action_defs(
        CORE_ACTION_DEFS, EDENFS_ACTION_DEFS
    ).items()
}


GENERATION = GenerationOptions(
    observable_target_kinds=observable_target_kinds,
    sample_observation_pair=_sample_observation_pair,
    held_inputs=HELD_INPUTS,
    held_descriptor=HELD_DESCRIPTOR,
    uncontended_sites=frozenset({LOOKUP_SITE}),
    forbid_overlap=forbid_overlap,
    produced_value=produced_value,
)
