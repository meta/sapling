# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Canary shard pin tool for ShardManager-managed services.

Called by a Conveyor CanaryNode's CustomScriptPlugin after startup monitoring.
It pins an explicit set of shards onto each canary task (via SM "canary mode"
== ``setShardPreference`` with ``PreferenceType.UserCanary``) so the canary
tasks serve real traffic during the monitoring window, then unpins them on exit.

Inputs are split between named CLI args (stable, set in the canary config) and
env vars (per-run, injected by the canary scheduler):

  CLI args:
    --service   ShardManager service name (e.g. mononoke.git_server)
    --scope     ShardManager scope        (e.g. global)
    --role      shard role to pin: primary | secondary (default: secondary)
    --shards    explicit list of SM shard-id strings to pin per canary task

The shards are distributed round-robin across all canary tasks (test and
control together), rather than pinning every shard onto every task: each task
serves a slice of the repos, and the test/control sides stay symmetric so the
canary counter comparison is meaningful.

  Env vars (set by servicefoundry/experimentplugin/chronos_utils.py):
    TEST_TARGET          comma-separated "handle/task_id" entries for canary tasks
                         e.g. "tsp_prn/mononoke/mononoke_git_server/0,.../1"
    CONTROL_TARGET       same format for control tasks
    EXPIRATION_TIMESTAMP unix epoch when the canary experiment expires
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal
import sys
import time
from typing import List, Optional, Tuple

from libfb.py.shardmanager.client_factory.SMClient import SMClient
from libfb.py.shardmanager.client_factory.SourceProvider import SourceProvider
from libfb.py.shardmanager.structs import EntityID, EntityType, SourceType
from shardmanager.smcli.param import PrefParamType


logger: logging.Logger = logging.getLogger(__name__)

# Stop pinning this many seconds before expiration so the cleanup (unpin) has
# time to run before the canary scheduler tears the task down.
DEFAULT_CLEANUP_MARGIN_SECS = 120
# Give SM a moment to actually move the pinned shard replicas onto the task
# before we start counting down the monitoring window.
DEFAULT_SETTLE_SECS = 60


def _parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--service",
        default="mononoke.git_server",
        help="ShardManager service name",
    )
    parser.add_argument(
        "--scope",
        default="global",
        help="ShardManager scope name",
    )
    parser.add_argument(
        "--role",
        default="secondary",
        choices=["primary", "secondary"],
        help="Shard role to pin (Secondary-role services use 'secondary')",
    )
    parser.add_argument(
        "--shards",
        nargs="+",
        required=True,
        metavar="SHARD_ID",
        help="SM shard-id strings to distribute across the canary tasks",
    )
    parser.add_argument(
        "--settle-secs",
        type=int,
        default=DEFAULT_SETTLE_SECS,
        help="Seconds to wait after pinning for shards to settle",
    )
    parser.add_argument(
        "--cleanup-margin-secs",
        type=int,
        default=DEFAULT_CLEANUP_MARGIN_SECS,
        help="Stop this many seconds before EXPIRATION_TIMESTAMP to allow unpin",
    )
    return parser.parse_args(argv)


def _parse_task_handles(target: str) -> List[str]:
    """Split a TEST_TARGET/CONTROL_TARGET value into "handle/task_id" entries."""
    return [h.strip() for h in target.split(",") if h.strip()]


def _distribute_shards(
    shards: List[str], handles: List[str]
) -> List[Tuple[str, List[str]]]:
    """Round-robin assign shards to handles.

    Returns (handle, shards) pairs, only for handles that received at least one
    shard. Pinning an empty preference list would evict all of a task's shards,
    so handles with no assigned shard are left untouched.
    """
    buckets: List[List[str]] = [[] for _ in handles]
    for i, shard in enumerate(shards):
        buckets[i % len(handles)].append(shard)
    return [(handles[i], buckets[i]) for i in range(len(handles)) if buckets[i]]


async def _resolve_server_id(
    client: SourceProvider, service: str, scope: str, handle: str
) -> int | None:
    """Resolve a TW task handle to its SM server id, or None if not found."""
    rows = await client.getServers(
        service=service,
        scope=scope,
        fields=["server_id"],
        entities=[EntityID(handle, EntityType.TW_TASK_HANDLE)],
    )
    server_ids = [row[0] for row in rows]
    if not server_ids:
        logger.warning("No SM server found for task handle: %s", handle)
        return None
    if len(server_ids) > 1:
        logger.warning(
            "Multiple SM servers (%s) for handle %s, using first",
            server_ids,
            handle,
        )
    return server_ids[0]


async def _wait_until(deadline: float, stop_event: asyncio.Event) -> None:
    """Sleep until `deadline` (epoch secs) or until stop_event is set."""
    while True:
        remaining = deadline - time.time()
        if remaining <= 0:
            return
        try:
            await asyncio.wait_for(stop_event.wait(), timeout=min(remaining, 30))
            logger.info("Received stop signal, ending canary period early")
            return
        except asyncio.TimeoutError:
            continue


async def _cleanup(
    client: SourceProvider, service: str, scope: str, pinned: List[int]
) -> None:
    """Disable SM canary on every server we pinned. Best-effort."""
    logger.info("Cleanup: disabling SM canary on %d server(s)", len(pinned))
    for server_id in pinned:
        try:
            await client.disableCanary(service, scope, server_id)
            logger.info("  Disabled canary on server %s", server_id)
        except Exception as e:
            logger.warning("  Failed to disable canary on server %s: %s", server_id, e)
    logger.info("Cleanup complete")


async def _pin_shards(
    client: SourceProvider,
    args: argparse.Namespace,
    assignment: List[Tuple[str, List[str]]],
) -> List[int]:
    """Enable SM canary for each (handle, shards) pair. Returns pinned server ids."""
    pref_param = PrefParamType()
    pinned: List[int] = []
    for handle, shards in assignment:
        server_id = await _resolve_server_id(client, args.service, args.scope, handle)
        if server_id is None:
            continue
        preferences = [
            pref_param.make_shard_preference(shard, args.role) for shard in shards
        ]
        try:
            await client.enableCanary(args.service, args.scope, server_id, preferences)
            pinned.append(server_id)
            logger.info(
                "Enabled SM canary on server %s (handle %s): %s",
                server_id,
                handle,
                ", ".join(shards),
            )
        except Exception as e:
            logger.warning(
                "Failed to enable canary on server %s (handle %s): %s",
                server_id,
                handle,
                e,
            )
    return pinned


def _read_deadline(args: argparse.Namespace) -> Optional[int]:
    """Compute the unpin deadline from EXPIRATION_TIMESTAMP, or None if invalid."""
    expiration = os.environ.get("EXPIRATION_TIMESTAMP", "").strip()
    if not expiration:
        logger.error("EXPIRATION_TIMESTAMP is not set")
        return None
    try:
        return int(expiration) - args.cleanup_margin_secs
    except ValueError:
        logger.error("EXPIRATION_TIMESTAMP is not an integer: %r", expiration)
        return None


async def _run(args: argparse.Namespace) -> int:
    # Distribute shards round-robin across all canary tasks (test + control) so
    # both sides serve real traffic and stay symmetric for the comparison.
    handles = _parse_task_handles(
        os.environ.get("TEST_TARGET", "")
    ) + _parse_task_handles(os.environ.get("CONTROL_TARGET", ""))
    if not handles:
        logger.error("No canary tasks: TEST_TARGET and CONTROL_TARGET are both empty")
        return 1

    deadline = _read_deadline(args)
    if deadline is None:
        return 1

    assignment = _distribute_shards(args.shards, handles)
    logger.info(
        "Distributing %d shard(s) as %s across %d canary task(s) of %s:%s",
        len(args.shards),
        args.role,
        len(handles),
        args.service,
        args.scope,
    )

    # Install signal handlers so SIGTERM/SIGINT end the wait gracefully and let
    # the finally block run cleanup (SIGKILL still can't be caught).
    stop_event = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop_event.set)

    pinned: List[int] = []
    async with SMClient(provider=SourceType.SM) as client:
        try:
            pinned = await _pin_shards(client, args, assignment)
            if not pinned:
                logger.error("Failed to pin shards on any canary server")
                return 1

            logger.info(
                "Pinned shards on %d server(s); settling up to %ds",
                len(pinned),
                args.settle_secs,
            )
            await _wait_until(time.time() + args.settle_secs, stop_event)

            logger.info("Holding pins until %d (epoch)", deadline)
            await _wait_until(deadline, stop_event)
            return 0
        finally:
            await _cleanup(client, args.service, args.scope, pinned)


def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="[%(asctime)s] %(levelname)s %(message)s",
    )
    args = _parse_args(sys.argv[1:])
    sys.exit(asyncio.run(_run(args)))


if __name__ == "__main__":
    main()
