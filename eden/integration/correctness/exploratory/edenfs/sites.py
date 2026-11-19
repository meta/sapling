# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""EdenFS fault sites and their local scheduling constraints.

Each `SITES` entry is one EdenFS `FaultInjector` checkpoint: its key
class (e.g. `TreeInode::checkout`), the action kinds a fault there can
target, its behaviors (block, delay, error), the daemon that carries it
(`public-master`, or `stack-build`: a daemon built from a revision with
checkpoints public master lacks), and whether its target's effect has
happened when it fires (`effect`; the comment above `SITES`). `validate`
adds the site's own RunSpec rules. `engage` returns an `EngagePlan`: the
actions that make the target actually reach the checkpoint, such as
warming or unloading inodes. `UNOBSERVABLE_PAIRS` lists the site and
target-kind pairs generation never composes.
"""

from __future__ import annotations

import itertools
import random
from collections.abc import Mapping
from pathlib import PurePosixPath
from typing import Any

from eden.integration.correctness.exploratory.core.runspec import (
    Action,
    finish_before,
    RunSpec,
    SpecError,
)
from eden.integration.correctness.exploratory.core.sites import (
    EngagePlan,
    POST_EFFECT,
    PRE_EFFECT,
    Site,
)
from eden.integration.correctness.exploratory.edenfs.fixture import REPLACEMENT_STRIDE
from eden.integration.correctness.repo import GeneratedRepo

CHECKOUT_BEGIN_SITE = "checkout-begin"
STATUS_CACHE_FILL_SITE = "status-cache-fill"
PENDING_WRITES_SITE = "wait-for-pending-writes"
LOOKUP_SITE = "fuse-lookup"
FETCH_TREE_SITE = "fetch-tree"
TREE_CHECKOUT_SITE = "tree-checkout"
OVERLAY_WRITE_SITE = "overlay-write"
INODEMAP_LOAD_SITE = "inodemap-load"
JOURNAL_RECORD_SITE = "journal-record"

# Generation never pairs these sites with these target kinds: in live runs
# the target did not reach the checkpoint (each value counts the seeds in
# which the armed fault never fired). Hand-written specs may still use them.
UNOBSERVABLE_PAIRS = {
    (TREE_CHECKOUT_SITE, "checkout"): "27 fault-miss seeds",
    (PENDING_WRITES_SITE, "read"): "23 fault-miss seeds",
    (FETCH_TREE_SITE, "status"): "15 fault-miss seeds",
    (STATUS_CACHE_FILL_SITE, "status"): "11 fault-miss seeds",
}


def observable_target_kinds(site_name: str, site: Site) -> frozenset[str]:
    """Return the kinds generation may target at `site_name`.

    That is the site's target kinds minus `UNOBSERVABLE_PAIRS`; validation of
    hand-written specs still accepts every target kind.
    """
    return frozenset(
        kind
        for kind in site.target_kinds
        if (site_name, kind) not in UNOBSERVABLE_PAIRS
    )


_TREE_WIDE_KINDS = frozenset({"checkout", "status", "status_counts", "sl_rm"})
_LOOKUP_WARMING_KINDS = frozenset({"write", "read", "getattr", "checkout"})
_STATUS_FILLING_KINDS = frozenset({"status", "status_counts", "sl_rm"})
_JOURNAL_ADVANCING_KINDS = frozenset({"write"})
_PATH_UPDATE_KINDS = frozenset({"write", "chmod"})
# Requests that can load a path's inode by number (`InodeMap::startChildLookup`).
_PATH_LOAD_KINDS = frozenset({"read", "write", "getattr", "chmod", "open"})


def _validate_contention(spec: RunSpec, install: Action) -> None:
    """Require installs for one registration key to have disjoint lifetimes."""
    remove_id = next(
        (
            action.id
            for action in spec.actions
            if action.kind == "fault_remove"
            and action.payload["fault_id"] == install.payload["fault_id"]
        ),
        install.payload["target_action"],
    )
    for other in spec.actions:
        if other.kind != "fault_install" or other.id == install.id:
            continue
        if registration_key(spec, install) != registration_key(spec, other):
            continue
        other_remove = next(
            (
                action.id
                for action in spec.actions
                if action.kind == "fault_remove"
                and action.payload["fault_id"] == other.payload["fault_id"]
            ),
            other.payload["target_action"],
        )
        if remove_id in finish_before(spec, other.id):
            continue
        if other_remove in finish_before(spec, install.id):
            continue
        raise SpecError(
            "at most one active fault may arm one fault key, got "
            f"{install.id!r} and {other.id!r}"
        )


def _validate_collateral(
    spec: RunSpec, install: Action, target: Action, matches: Any
) -> None:
    """Keep every other action the armed key can match away from the armed fault.

    A one-shot fault fires on the first request with its key, and only its
    target's failure is excused, so another such request must finish before
    the install starts or start once the target has consumed the fault (its
    parked witness for a block, its completion otherwise).
    """
    consumed_id, remove_id = _fault_partners(spec, install, target)
    before_install = finish_before(spec, install.id)
    for action in spec.actions:
        if action.id == target.id or action.id in before_install:
            continue
        before_action = finish_before(spec, action.id)
        if not matches(action, target) or {consumed_id, remove_id} & before_action:
            continue
        raise SpecError(
            f"action {action.id!r} can hit the key of fault "
            f"{install.payload['fault_id']!r} while it is armed"
        )


def _same_path_update(action: Action, target: Action) -> bool:
    return action.kind in _PATH_UPDATE_KINDS and action.payload.get(
        "path"
    ) == target.payload.get("path")


def _same_path_load(action: Action, target: Action) -> bool:
    return action.kind in _PATH_LOAD_KINDS and action.payload.get(
        "path"
    ) == target.payload.get("path")


def _records_journal(action: Action, target: Action) -> bool:
    """A FORCE checkout records its conflicting paths (`EdenMount::checkout`)."""
    return _same_path_update(action, target) or (
        action.kind == "checkout" and action.payload.get("mode") == "force"
    )


def _any_checkout(action: Action, _target: Action) -> bool:
    return action.kind == "checkout"


def _waits_for_pending_writes(action: Action, _target: Action) -> bool:
    """Every checkout (DRY_RUN included), status, and synced Thrift read waits
    for pending writes (`EdenMount::co_waitForPendingWrites` callers)."""
    return action.kind in ("checkout", "status") or (
        action.kind == "read"
        and action.payload.get("channel") == "thrift"
        and action.payload.get("sync_timeout_seconds") not in (None, 0)
    )


def _tree_wide(action: Action, _target: Action) -> bool:
    return action.kind in _TREE_WIDE_KINDS


def _validate_path_keyed(spec: RunSpec, install: Action, target: Action) -> None:
    _validate_contention(spec, install)
    _validate_collateral(spec, install, target, _same_path_update)


def _validate_journal_record(spec: RunSpec, install: Action, target: Action) -> None:
    _validate_contention(spec, install)
    _validate_collateral(spec, install, target, _records_journal)


def _validate_checkout_begin(spec: RunSpec, install: Action, target: Action) -> None:
    _validate_contention(spec, install)
    _validate_collateral(spec, install, target, _any_checkout)


def _fault_partners(spec: RunSpec, install: Action, target: Action) -> tuple[str, str]:
    fault_id = install.payload["fault_id"]
    partners = [
        action
        for action in spec.actions
        if action.id != install.id and action.payload.get("fault_id") == fault_id
    ]
    await_action = next(
        (
            action
            for action in partners
            if action.kind == "fault_await"
            and (target.id, "started") in action.requires
        ),
        None,
    )
    remove_action = next(action for action in partners if action.kind == "fault_remove")
    return (
        await_action.id if await_action is not None else target.id
    ), remove_action.id


def _lookup_basename(spec: RunSpec, target: Action) -> str | None:
    if target.kind == "read" and target.payload.get("channel") == "fuse":
        path = target.payload.get("path")
        return PurePosixPath(path).name if path else None
    return None


def registration_key(spec: RunSpec, install: Action) -> tuple[str, str | None]:
    """Return the daemon registration identity armed by an install."""
    site = SITES[install.payload["site"]]
    if site.fixed_key is not None:
        return site.key_class, site.fixed_key
    if install.payload.get("site") == TREE_CHECKOUT_SITE:
        key = install.payload.get("key")
        return site.key_class, key if isinstance(key, str) else None
    if site.name_keyed:
        target = spec.action(install.payload["target_action"])
        return site.key_class, _lookup_basename(spec, target)
    return site.key_class, None


def _validate_lookup_channel(target: Action) -> None:
    if target.kind == "read" and target.payload.get("channel") != "fuse":
        raise SpecError("fuse-lookup target must use the fuse channel")


def _validate_lookup_cold_setup(spec: RunSpec) -> None:
    """Reject setup profiles that warm all lookup targets before actions run."""
    setup_profile = spec.environment.target["setup_profile"]
    if setup_profile.get("warm_readdir", False):
        raise SpecError("fuse-lookup target must be cold: warm_readdir is enabled")
    if setup_profile.get("aged_overlay", 0) > 0:
        raise SpecError("fuse-lookup target must be cold: aged_overlay is enabled")


def _lookup_target_path(spec: RunSpec, target: Action) -> str | None:
    """Return the register path whose lookup is parked by the target action."""
    path = target.payload.get("path")
    return path if isinstance(path, str) else None


def _validate_lookup_target_path(spec: RunSpec, target: Action, remove_id: str) -> None:
    """Reject another actor touching the lookup path before fault removal."""
    path = _lookup_target_path(spec, target)
    if path is None:
        return
    owners = {
        action_id: actor.id for actor in spec.actors for action_id in actor.actions
    }
    for action in spec.actions:
        if action.id == target.id or action.kind not in {"write", "read", "getattr"}:
            continue
        if action.payload.get("path") != path:
            continue
        if owners.get(action.id) == owners.get(target.id):
            continue
        if remove_id in finish_before(spec, action.id):
            continue
        raise SpecError(
            f"lookup target path {path!r} is touched by other actor "
            f"{action.id!r} before fault_remove"
        )


def _validate_lookup(spec: RunSpec, install: Action, target: Action) -> None:
    _validate_lookup_channel(target)
    _validate_contention(spec, install)
    _validate_lookup_cold_setup(spec)
    await_id, remove_id = _fault_partners(spec, install, target)
    _validate_lookup_target_path(spec, target, remove_id)
    name = _lookup_basename(spec, target)
    if name is not None:
        before_install = finish_before(spec, install.id)
        for action in spec.actions:
            if action.id == target.id:
                continue
            path = action.payload.get("path")
            explicit = bool(path and PurePosixPath(path).name == name)
            if not explicit and action.kind not in _TREE_WIDE_KINDS:
                continue
            if action.id in before_install:
                continue
            before_action = finish_before(spec, action.id)
            if remove_id in before_action or await_id in before_action:
                continue
            raise SpecError(
                f"action {action.id!r} can look up {name!r} while fault "
                f"{install.payload['fault_id']!r} is armed"
            )

    path = target.payload.get("path")
    if target.kind != "read" or target.payload.get("channel") != "fuse" or not path:
        return
    earlier = finish_before(spec, target.id)
    for action in spec.actions:
        if action.id == target.id or action.id not in earlier:
            continue
        if action.kind not in _LOOKUP_WARMING_KINDS:
            continue
        if action.payload.get("path") != path:
            continue
        raise SpecError(
            f"lookup target {target.id!r} reads warmed register {path!r} "
            f"(touched by {action.id!r} before it starts)"
        )


def _validate_cache_fill(spec: RunSpec, install: Action, target: Action) -> None:
    _validate_contention(spec, install)
    if target.payload.get("channel") != "sl":
        raise SpecError("status-cache-fill target must use the sl channel")
    await_id, remove_id = _fault_partners(spec, install, target)
    dirt_ids = {
        action.id for action in spec.actions if action.kind in _JOURNAL_ADVANCING_KINDS
    }
    # A probe that finishes before every dirt (before the install when
    # nothing dirties) fills the cache with a view the target still sees.
    settled = (
        set.intersection(*(finish_before(spec, dirt_id) for dirt_id in dirt_ids))
        if dirt_ids
        else finish_before(spec, install.id)
    )
    for action in spec.actions:
        if action.id == target.id or action.kind not in _STATUS_FILLING_KINDS:
            continue
        before_probe = finish_before(spec, action.id)
        if remove_id in before_probe or await_id in before_probe:
            continue
        if action.id in settled:
            continue
        raise SpecError(
            f"probe {action.id!r} fills the status cache after dirt but before "
            f"install {install.id!r} arms"
        )


def _validate_pending_writes(spec: RunSpec, install: Action, target: Action) -> None:
    _validate_contention(spec, install)
    if target.payload.get("channel") != "thrift":
        raise SpecError("wait-for-pending-writes target must use the thrift channel")
    if target.payload.get("sync_timeout_seconds") in (None, 0):
        raise SpecError("wait-for-pending-writes target must use nonzero sync")
    _validate_collateral(spec, install, target, _waits_for_pending_writes)


def _validate_fetch_binding(spec: RunSpec, install: Action, target: Action) -> None:
    """Validate fields that bind a status request to the root-tree site."""
    if install.payload.get("ordinal", 1) != 1:
        raise SpecError("fetch-tree supports only ordinal 1")
    repository = spec.environment.target.get("repository")
    if type(repository) is not dict:
        raise SpecError("fetch-tree requires a repository fixture")
    commit_count = repository.get("commit_count")
    if type(commit_count) is not int or commit_count <= 0:
        raise SpecError("fetch-tree requires a nonempty commit fixture")
    channel = target.payload.get("channel")
    revision = target.payload.get("revision")
    if channel not in {"sl", "thrift"}:
        raise SpecError("fetch-tree target must use sl or thrift")
    if revision is not None and (
        type(revision) is not int or not 0 <= revision < commit_count
    ):
        raise SpecError("fetch-tree revision must name a fixture commit")
    current = commit_count - 1
    if channel == "thrift" and revision not in (None, current):
        raise SpecError("fetch-tree thrift revision must be the current parent")
    if spec.environment.target.get("fixture_source") != "backing":
        raise SpecError("fetch-tree requires the backing fixture")


def _validate_fetch_isolation(spec: RunSpec, install: Action, target: Action) -> None:
    """Keep competing root fetches outside the install-to-witness interval."""
    _, remove_id = _fault_partners(spec, install, target)
    if target.id in finish_before(spec, remove_id):
        raise SpecError("fetch-tree removal must not wait on the parked target")
    _validate_collateral(spec, install, target, _tree_wide)


def _validate_fetch_tree(spec: RunSpec, install: Action, target: Action) -> None:
    """Reserve one valid root fetch through its witnessed hit."""
    _validate_contention(spec, install)
    _validate_fetch_binding(spec, install, target)
    _validate_fetch_isolation(spec, install, target)


def _validate_tree_checkout(spec: RunSpec, install: Action, target: Action) -> None:
    """Bind one Thrift checkout to a relative directory-key subtree."""
    _validate_contention(spec, install)
    if target.payload.get("via") != "thrift":
        raise SpecError("tree-checkout target must use thrift")
    key = install.payload.get("key")
    if type(key) is not str or not key:
        raise SpecError("tree-checkout requires a nonempty key")
    path = PurePosixPath(key)
    if path.is_absolute() or ".." in path.parts:
        raise SpecError("tree-checkout key must be mount-relative")
    ordinal = install.payload.get("ordinal", 1)
    if type(ordinal) is not int or ordinal < 1:
        raise SpecError("tree-checkout ordinal must be positive")


def _validate_inodemap_load(spec: RunSpec, install: Action, target: Action) -> None:
    """The checkpoint fires only when the target inode loads on demand."""
    _validate_contention(spec, install)
    _validate_collateral(spec, install, target, _same_path_load)
    path = target.payload.get("path")
    if not isinstance(path, str):
        return
    earlier = finish_before(spec, install.id)
    for action in spec.actions:
        if action.id == target.id or action.id not in earlier:
            continue
        if action.kind not in {"write", "read", "getattr"}:
            continue
        if action.payload.get("path") != path:
            continue
        raise SpecError(
            f"inodemap-load target {path!r} is loaded by {action.id!r} "
            f"before install {install.id!r} arms"
        )
    target_parts = PurePosixPath(path).parts
    for action in spec.actions:
        if action.kind != "unload_inodes" or action.id not in earlier:
            continue
        unload_path = action.payload.get("path")
        if not isinstance(unload_path, str) or not unload_path:
            continue
        unload_parts = PurePosixPath(unload_path).parts
        if target_parts[: len(unload_parts)] == unload_parts:
            return
    raise SpecError(
        f"inodemap-load target {path!r} has no unload_inodes covering it "
        f"before install {install.id!r} arms"
    )


TREE_CHECKOUT_KEY = "tree/a1"


def _dirt_path(ctx: Mapping[str, Any], rng: random.Random) -> str:
    """Return one sampled write path that dirties the status view."""
    return rng.choice(list(ctx["environment"]["repository"]["registers"]))


def _engage_fetch_tree(rng: random.Random, ctx: Mapping[str, Any]) -> EngagePlan:
    """Park a revisionless sl status after one pre-install write."""
    return EngagePlan(
        {},
        (("write", {"path": _dirt_path(ctx, rng), "value_id": "e0"}),),
        {"channel": "sl", "revision": None},
    )


def _engage_status_cache_fill(rng: random.Random, ctx: Mapping[str, Any]) -> EngagePlan:
    """Complete one write before the install; the probe stays sl."""
    return EngagePlan(
        {},
        (("write", {"path": _dirt_path(ctx, rng), "value_id": "e0"}),),
        {"channel": "sl"},
    )


def _engage_pending_writes(rng: random.Random, ctx: Mapping[str, Any]) -> EngagePlan:
    """Read over thrift with nonzero sync after writing the same path."""
    path = _dirt_path(ctx, rng)
    return EngagePlan(
        {},
        (("write", {"path": path, "value_id": "e0"}),),
        {"path": path, "channel": "thrift", "sync_timeout_seconds": 60},
    )


def _moves_key(layout: GeneratedRepo, revision: int) -> bool:
    """Return whether entering `revision` moves a generated path under the install key."""
    return any(
        layout.file_at(index, revision).path.startswith(TREE_CHECKOUT_KEY + "/")
        for index in itertools.islice(layout.changed_indices(revision), 2)
    )


def _engage_tree_checkout(rng: random.Random, ctx: Mapping[str, Any]) -> EngagePlan:
    """Check out a non-tip revision that moves the install key directory.

    A revision differs from the tip exactly at the indexes that enter a
    later revision, and two consecutive indexes of one revision's stride
    cover both actors, so each candidate costs two indexed queries.
    """
    repository = ctx["environment"]["repository"]
    tip = repository["commit_count"] - 1
    layout = GeneratedRepo(
        ctx["seed"], repository["file_count"], repository["commit_count"]
    )
    candidates = [
        revision
        for revision in range(tip)
        if any(_moves_key(layout, later) for later in range(revision + 1, tip + 1))
    ]
    revision = rng.choice(candidates) if candidates else tip - 1
    return EngagePlan({}, (), {"revision": revision, "via": "thrift"})


def _engage_checkout_begin(_rng: random.Random, _ctx: Mapping[str, Any]) -> EngagePlan:
    """Checkouts need no preconditions, so engagement stays empty."""
    return EngagePlan({}, (), {})


def _engage_inodemap_load(rng: random.Random, ctx: Mapping[str, Any]) -> EngagePlan:
    """Target an unloaded generated path with a setup that keeps it cold.

    Prefer a path present at every revision, so the target exists live no
    matter which revision a baseline checkout leaves behind: an index whose
    replacement revision lies past the tip never moves. Choosing a residue
    and then a multiple keeps the choice O(1) in `file_count`.
    """
    repository = ctx["environment"]["repository"]
    count, commits = repository["file_count"], repository["commit_count"]
    layout = GeneratedRepo(ctx["seed"], count, commits)
    tip = commits - 1
    residues = [
        residue for residue in range(tip, REPLACEMENT_STRIDE) if residue < count
    ]
    if residues:
        residue = rng.choice(residues)
        index = residue + REPLACEMENT_STRIDE * rng.randrange(
            (count - 1 - residue) // REPLACEMENT_STRIDE + 1
        )
    else:
        index = rng.randrange(count)
    path = layout.file_at(index, tip).path
    return EngagePlan(
        {"materialize": False, "warm_readdir": False},
        (("unload_inodes", {"path": str(PurePosixPath(path).parent)}),),
        {"path": path},
    )


# Overlay-write, journal-record, and fuse-lookup have no engage plan, so they
# compose unengaged; inodemap-load engages a cold generated path above. Effect
# positions: overlay-write checks before `pwritev`
# (`OverlayFileAccess.cpp:345-356`), so an error there lands nothing;
# journal-record checks after the bytes are in the overlay
# (`FileInode.cpp:1493-1508`, `Journal.cpp:128-134`). checkout-begin fires
# after `beginCheckout` entered CheckoutInProgress but before start; errors
# restore prior state (D129789564). tree-checkout is inside the started walk,
# so a checkout it fails may have applied in part;
# fuse-lookup fires only on a FUSE_LOOKUP request (`FuseChannel::fuseLookup`),
# never inside a checkout's walk, and is the first statement of
# `FuseDispatcherImpl::lookup`, so an error there loads nothing; the
# read-only sites never precede an effect.
SITES: dict[str, Site] = {
    OVERLAY_WRITE_SITE: Site(
        key_class="overlay-write",
        target_kinds=frozenset({"write"}),
        behaviors=frozenset({"block", "error"}),
        daemon="stack-build",
        effect=PRE_EFFECT,
        validate=_validate_path_keyed,
    ),
    INODEMAP_LOAD_SITE: Site(
        key_class="inodemap-load",
        target_kinds=frozenset({"getattr"}),
        behaviors=frozenset({"block", "error"}),
        daemon="stack-build",
        effect=PRE_EFFECT,
        validate=_validate_inodemap_load,
        engage=_engage_inodemap_load,
    ),
    JOURNAL_RECORD_SITE: Site(
        key_class="journal-record",
        target_kinds=frozenset({"write"}),
        behaviors=frozenset({"block", "error"}),
        daemon="stack-build",
        effect=POST_EFFECT,
        validate=_validate_journal_record,
    ),
    CHECKOUT_BEGIN_SITE: Site(
        key_class="checkout",
        target_kinds=frozenset({"checkout"}),
        behaviors=frozenset({"block", "delay", "error"}),
        daemon="public-master",
        effect=POST_EFFECT,
        validate=_validate_checkout_begin,
        engage=_engage_checkout_begin,
    ),
    STATUS_CACHE_FILL_SITE: Site(
        key_class="scmStatusCache",
        target_kinds=frozenset({"status"}),
        behaviors=frozenset({"block", "delay", "error"}),
        daemon="public-master",
        effect=PRE_EFFECT,
        validate=_validate_cache_fill,
        fixed_key="blocking setValue",
        engage=_engage_status_cache_fill,
    ),
    PENDING_WRITES_SITE: Site(
        key_class="waitForPendingWrites",
        target_kinds=frozenset({"read"}),
        behaviors=frozenset({"block", "delay", "error"}),
        daemon="public-master",
        effect=PRE_EFFECT,
        validate=_validate_pending_writes,
        fixed_key="",
        engage=_engage_pending_writes,
    ),
    LOOKUP_SITE: Site(
        key_class="FuseDispatcherImpl::lookup",
        target_kinds=frozenset({"read"}),
        behaviors=frozenset({"block", "delay", "error"}),
        daemon="stack-build",
        effect=PRE_EFFECT,
        validate=_validate_lookup,
        name_keyed=True,
    ),
    FETCH_TREE_SITE: Site(
        key_class="SaplingBackingStore::getRootTree",
        target_kinds=frozenset({"status"}),
        behaviors=frozenset({"block"}),
        daemon="public-master",
        effect=PRE_EFFECT,
        validate=_validate_fetch_tree,
        witness_any=True,
        engage=_engage_fetch_tree,
    ),
    TREE_CHECKOUT_SITE: Site(
        key_class="TreeInode::checkout",
        target_kinds=frozenset({"checkout"}),
        behaviors=frozenset({"block"}),
        daemon="public-master",
        effect=POST_EFFECT,
        validate=_validate_tree_checkout,
        name_keyed=True,
        witness_any=True,
        engage=_engage_tree_checkout,
    ),
}
