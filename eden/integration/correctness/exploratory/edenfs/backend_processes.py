# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Owned child processes of the live backend: leases, directory streams, and kills.

Every child runs in its own process group registered with the environment's
process registry; ``_kill_process_group`` empties a group before returning.
"""

from __future__ import annotations

import functools
import json
import os
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from typing import Any, TYPE_CHECKING

from eden.integration.correctness.exploratory.core.containment import _runner_env
from eden.integration.correctness.exploratory.core.history import FAIL, INFO, OK
from eden.integration.correctness.exploratory.core.processes import (
    confirm_gone,
    kill_group,
    spawn,
)
from eden.integration.correctness.exploratory.core.runspec import Action
from eden.integration.correctness.exploratory.core.scheduler import (
    BackendResult,
    HarnessFailure,
    INVALID_BACKEND_RESPONSE,
)

if TYPE_CHECKING:
    from eden.integration.correctness.exploratory.edenfs.backends import LiveBackend

_KILL_WAIT_SECONDS = 30.0

# How long a reader may outlive its lease before the lease completes
# `info`: past this its walk waits on something, such as a request a block
# fault holds, that it cannot finish without.
_LEASE_DRAIN_SECONDS = 10.0
_LATE_PIPE_SECONDS = 1.0
_LATE_STDERR_CHARS = 2000
LEASE_OVERRUN = "lease-overrun"


@functools.cache
def _spawner() -> ThreadPoolExecutor:
    """Return the one long-lived thread that forks readers.

    A child's parent-death signal fires when the thread that forked it
    exits, so a reader forked by its action's scheduler thread would die
    when its lease completed.
    """
    return ThreadPoolExecutor(max_workers=1, thread_name_prefix="reader-spawner")


_BACKGROUND_READ = """
import ctypes, os, sys, time
if ctypes.CDLL(None, use_errno=True).prctl(1, 9) != 0:
    raise OSError(ctypes.get_errno(), 'parent-death signal')
if os.getppid() != int(sys.argv[3]):
    sys.exit(1)
def failed(error):
    raise error
deadline = time.monotonic() + float(sys.argv[2])
while time.monotonic() < deadline:
    for directory, directories, files in os.walk(sys.argv[1], onerror=failed):
        pass
    print('sweep', flush=True)
    time.sleep(0.01)
"""


# No parent-death signal: the scheduler thread that spawns the stream
# exits at the pause, and PDEATHSIG fires on thread exit. A closed stdin
# (runner gone) ends the pause instead, and unwind kills the group.
_READDIR_STREAM = """
import json, os, sys
fd = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY)
def position():
    with open(f'/proc/self/fdinfo/{fd}') as info:
        return int(next(line for line in info if line.startswith('pos:')).split()[1])
entries = []
with os.scandir(fd) as stream:
    for entry in stream:
        entries.append(entry.name)
        if len(entries) == int(sys.argv[2]):
            break
    pos_at_pause = position()
    print(json.dumps({'paused_at': len(entries), 'pos_at_pause': pos_at_pause}), flush=True)
    if not sys.stdin.readline():
        sys.exit(2)
    pos_at_end = pos_at_pause
    for entry in stream:
        entries.append(entry.name)
        pos_at_end = position()
print(json.dumps({'entries': entries, 'pos_at_pause': pos_at_pause, 'pos_at_end': pos_at_end}), flush=True)
"""


def _kill_process_group(
    process: subprocess.Popen[Any], pgid: int, *, exited: bool = False
) -> bool:
    """SIGKILL one owned group, return only after it is empty, and say whether the leader had exited.

    ``exited`` says the leader was already reaped. A group already gone
    is success when ``exited`` or when ``process.poll()``, checked again
    after killpg found no group, shows the leader exited in between;
    otherwise it is a harness failure.
    """
    if not kill_group(pgid):
        if exited or process.poll() is not None:
            return True
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    if not confirm_gone(process, time.monotonic() + _KILL_WAIT_SECONDS):
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    return exited


def background_read(backend: LiveBackend, action: Action) -> BackendResult:
    """Keep one registered directory-walking process alive for the lease.

    A reader still running `_LEASE_DRAIN_SECONDS` after its lease ends
    completes the lease `info`; see `drain_overrun_readers`.
    """
    root = (backend._env.mount_path / action.payload["root"]).resolve(strict=True)
    if not root.is_relative_to(backend._env.mount_path.resolve()) or not root.is_dir():
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    argv = [
        sys.executable,
        "-c",
        _BACKGROUND_READ,
        str(root),
        str(action.payload["seconds"]),
        str(os.getpid()),
    ]
    with backend._reader_lock:
        if backend._readers_closed:
            raise HarnessFailure(INVALID_BACKEND_RESPONSE)
        future = _spawner().submit(
            spawn,
            backend._env.processes,
            action.id,
            argv,
            env=_runner_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        process = future.result()
        backend._readers[action.id] = process
    overran = False
    try:
        stdout, stderr = process.communicate(
            timeout=action.payload["seconds"] + _LEASE_DRAIN_SECONDS
        )
    except subprocess.TimeoutExpired:
        overran = True
        with backend._reader_lock:
            backend._overrun_readers.add(action.id)
        return BackendResult(INFO, {"reason": LEASE_OVERRUN, "pgid": process.pid})
    finally:
        if not overran:
            retire_stream(backend, action.id, process)
    return BackendResult(
        OK if process.returncode == 0 else FAIL,
        {
            "pgid": process.pid,
            "returncode": process.returncode,
            "stdout": stdout,
            "stderr": stderr,
            "sweeps": stdout.count("sweep\n"),
        },
    )


def _killed_by_spec(backend: LiveBackend, action_id: str) -> bool:
    return any(
        other.kind == "kill_process" and other.payload["target_action"] == action_id
        for other in backend._spec.actions
    )


def drain_overrun_readers(
    backend: LiveBackend,
) -> tuple[int, dict[str, int | None], dict[str, str]]:
    """Reap every overrun reader once faults are released; return failures, exits, and stderr.

    An overrun reader completed its lease `info` with no exit facts and
    stayed owned and running. All readers share one `_LEASE_DRAIN_SECONDS`
    wait for their exits; readers that exited but whose pipes were not yet
    read share at most `_LATE_PIPE_SECONDS` more. `exits` maps each to its
    late exit status, None for one still running, which is left to the
    supervisor's run-dir close. A failure is a reader still running or one
    with a nonzero exit other than SIGKILL after a spec `kill_process`
    actually signalled it (not one that found it already exited); the last
    `_LATE_STDERR_CHARS` of a nonzero exit's stderr are returned with it.
    """
    with backend._reader_lock:
        overrun = [(r, backend._readers[r]) for r in sorted(backend._overrun_readers)]
        signalled = dict(backend._signalled_readers)
    deadline = time.monotonic() + _LEASE_DRAIN_SECONDS
    pipe_deadline = deadline + _LATE_PIPE_SECONDS
    exits: dict[str, int | None] = {}
    stderr_tails: dict[str, str] = {}
    failures = 0
    for action_id, process in overrun:
        try:
            _stdout, stderr = process.communicate(
                timeout=max(0.0, deadline - time.monotonic())
            )
        except subprocess.TimeoutExpired:
            if process.poll() is None:
                exits[action_id] = None
                failures += 1
                continue
            stderr = _exited_stderr(process, pipe_deadline)
        exits[action_id] = process.returncode
        planned = (
            process.returncode == -signal.SIGKILL
            and signalled.get(action_id) == process.pid
        )
        failures += process.returncode != 0 and not planned
        if process.returncode != 0 and stderr:
            stderr_tails[action_id] = stderr[-_LATE_STDERR_CHARS:]
        retire_stream(backend, action_id, process)
        with backend._reader_lock:
            backend._overrun_readers.discard(action_id)
    return failures, exits, stderr_tails


def _exited_stderr(process: subprocess.Popen[str], pipe_deadline: float) -> str:
    """Read an exited reader's remaining stderr by `pipe_deadline`; "" if its pipes stay open."""
    try:
        return process.communicate(timeout=max(0.0, pipe_deadline - time.monotonic()))[
            1
        ]
    except subprocess.TimeoutExpired:
        return ""


def spawn_owned(
    backend: LiveBackend, action: Action, argv: list[str]
) -> subprocess.Popen[str]:
    """Start one registered child in its own group, pipes on every end."""
    with backend._reader_lock:
        if backend._readers_closed:
            raise HarnessFailure(INVALID_BACKEND_RESPONSE)
        process = spawn(
            backend._env.processes,
            action.id,
            [sys.executable, "-c", *argv],
            env=_runner_env(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        backend._readers[action.id] = process
    return process


def readdir_stream(backend: LiveBackend, action: Action) -> BackendResult:
    """Open one directory stream and complete once its child has paused."""
    root = (backend._env.mount_path / action.payload["path"]).resolve(strict=True)
    if not root.is_relative_to(backend._env.mount_path.resolve()) or not root.is_dir():
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    process = spawn_owned(
        backend,
        action,
        [_READDIR_STREAM, str(root), str(action.payload["pause_after"])],
    )
    stdout = process.stdout
    line = stdout.readline() if stdout is not None else ""
    if not line:
        retire_stream(backend, action.id, process)
        return stream_died(backend, action.id, "stream exited before pausing", "")
    return BackendResult(OK, {"pgid": process.pid, **json.loads(line)})


def retire_stream(
    backend: LiveBackend, action_id: str, process: subprocess.Popen[str]
) -> None:
    if process.poll() is None:
        _kill_process_group(process, process.pid)
    with backend._reader_lock:
        backend._readers.pop(action_id)
        backend._signalled_readers.pop(action_id, None)
        backend._env.processes.retire(action_id, process)


def stream_died(
    backend: LiveBackend, stream_id: str, reason: str, stderr: str
) -> BackendResult:
    """A dead stream is a FAIL only when a spec kill_process aimed at it."""
    if not _killed_by_spec(backend, stream_id):
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    return BackendResult(FAIL, {"reason": reason, "stderr": stderr})


def readdir_resume(backend: LiveBackend, action: Action) -> BackendResult:
    """Release a paused stream and report the names it drained.

    The child reports the directory fd's ``pos`` at the pause and after
    the last drained entry (scandir rewinds an fd it exhausts); equal
    values mean the pause never straddled a getdents call, so the
    stream proves nothing and the run is a harness failure.
    """
    stream_id = action.payload["stream"]
    with backend._reader_lock:
        process = backend._readers.get(stream_id)
    if process is None:
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    try:
        stdout, stderr = process.communicate("go\n")
    except (OSError, ValueError):
        stdout, stderr = "", ""
    finally:
        retire_stream(backend, stream_id, process)
    if process.returncode != 0 or not stdout:
        return stream_died(
            backend, stream_id, f"stream exited {process.returncode}", stderr
        )
    facts = json.loads(stdout)
    if facts["pos_at_end"] == facts["pos_at_pause"]:
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    return BackendResult(OK, {"stream": stream_id, **facts})


def _retired_kill(target_id: str, pgid: int | None) -> BackendResult:
    """Return the kill of a target whose owner retired it; a target never spawned fails."""
    if pgid is None:
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    return BackendResult(
        OK,
        {
            "target_action": target_id,
            "pgid": pgid,
            "group_empty": False,
            "already_exited": True,
        },
    )


def kill_process(backend: LiveBackend, action: Action) -> BackendResult:
    """Kill the target action's group after its spawn-relative delay.

    A target that already exited before the kill (``poll()`` is not None),
    or that exits before killpg reaches its group, is ``ok`` with
    ``already_exited: true``; any survivors in its group are still killed.
    An sl process is killed by its ``SaplingProcesses`` owner, which keeps
    its leader's one reap, so it is never polled here. A target its owner
    already retired is ``already_exited`` and is not signalled: its pgid may
    have been reused, so nothing checks its group (``group_empty: false``).
    """
    target_id = action.payload["target_action"]
    registry = backend._env.processes
    owned = registry.wait(target_id, backend._spec.budgets.runner_deadline_seconds)
    if owned is None:
        return _retired_kill(target_id, registry.finished(target_id))
    deadline = owned.spawned_at + action.payload["after_ms"] / 1000.0
    remaining = deadline - time.monotonic()
    if remaining > 0:
        time.sleep(remaining)
    current = registry.lookup(target_id)
    if current is None:
        return _retired_kill(target_id, registry.finished(target_id))
    if current != owned.pgid:
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)
    already_exited = backend._sl.kill(target_id, _KILL_WAIT_SECONDS)
    if already_exited is None:
        already_exited = _kill_process_group(
            owned.process, owned.pgid, exited=owned.process.poll() is not None
        )
    facts: dict[str, Any] = {
        "target_action": target_id,
        "pgid": owned.pgid,
        "group_empty": True,
    }
    if already_exited:
        facts["already_exited"] = True
    else:
        with backend._reader_lock:
            if backend._readers.get(target_id) is owned.process:
                backend._signalled_readers[target_id] = owned.pgid
    return BackendResult(OK, facts)
