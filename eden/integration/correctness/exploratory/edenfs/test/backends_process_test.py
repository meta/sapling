# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Tests for the backend's owned child processes: kills, leases, and directory streams."""

from __future__ import annotations

import errno
import io
import json
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest import mock

from eden.integration.correctness.exploratory.core.containment import _runner_env
from eden.integration.correctness.exploratory.core.facts import (
    decode_result,
    ResultProblem,
)
from eden.integration.correctness.exploratory.core.history import (
    FAIL,
    INFO,
    InMemorySink,
    OK,
)
from eden.integration.correctness.exploratory.core.runspec import (
    Action,
    Actor,
    Budgets,
    DEADLINE_OPERATION,
    FINISHED,
    RunSpec,
    STARTED,
    to_json,
)
from eden.integration.correctness.exploratory.core.scheduler import (
    BackendResult,
    COMPLETED,
    HarnessFailure,
    INVALID_BACKEND_RESPONSE,
    run_actions,
)
from eden.integration.correctness.exploratory.core.target import ProcessRegistry
from eden.integration.correctness.exploratory.edenfs import backend_processes
from eden.integration.correctness.exploratory.edenfs.backend_processes import (
    _kill_process_group,
)
from eden.integration.correctness.exploratory.edenfs.backends import LiveBackend
from eden.integration.correctness.exploratory.edenfs.environment import Environment
from eden.integration.correctness.exploratory.edenfs.target import RECORDING_TARGET
from eden.integration.correctness.exploratory.edenfs.test.backends_fixtures import (
    _BlockingProcess,
    _CannedSl,
    _FakeMountRepo,
    _native_spec,
    _REAL_POPEN,
    _session_leader,
    _spec,
    REGISTERS,
)


# Real seconds before a test gives up waiting; a hang guard, not a measurement.
_HANG_GUARD = 60.0


def _reap(process: subprocess.Popen[Any]) -> None:
    if process.poll() is None:
        process.kill()
        process.wait(timeout=_HANG_GUARD)


def _state(pid: int) -> str:
    """Return the process state letter from `/proc/<pid>/status`."""
    with open(f"/proc/{pid}/status") as status:
        line = next(line for line in status if line.startswith("State:"))
    return line.split()[1]


class _NativeMountRepo(_FakeMountRepo):
    """Mount-repo stub exposing the command fields used by native actions."""

    def __init__(self) -> None:
        super().__init__({REGISTERS[0]: "M"})
        self.path = "/mount"
        self.hg_bin = "/bin/sl"
        self.hg_environment = {"HGPLAIN": "1"}


class _NativeEnv(Environment):
    """Environment stub with commits and an action-process registry."""

    def __init__(self) -> None:
        self._processes = ProcessRegistry()
        self._mount_repo = cast(Any, _NativeMountRepo())
        self._commits = (mock.Mock(node="node0"), mock.Mock(node="node1"))


class NativeProcessBackendTest(unittest.TestCase):
    def test_owned_daemon_keeps_unexpected_crash_logging_synchronous(self) -> None:
        """Unexpected crashes retain flushed daemon logs for their exit reports."""
        env = Environment(
            file_count=2, commit_count=2, registers=REGISTERS, sink=InMemorySink()
        )
        try:
            env.setup()
            with env.get_thrift_client() as client:
                config = json.loads(cast(Any, client).getOption("logging"))
            self.assertEqual(config["handlers"]["default"]["options"]["async"], "false")
        finally:
            env.close()

    def _reader(
        self, seconds: float = 0.1, planned_kill: bool = True
    ) -> tuple[Any, RunSpec, LiveBackend]:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "tree").mkdir()
        (root / "tree" / "entry").write_bytes(b"data")
        env = SimpleNamespace(
            mount_path=root, processes=ProcessRegistry(), setup_facts={}
        )
        action = Action(
            "r",
            "background_read",
            "operation",
            payload={"root": "tree", "seconds": seconds},
        )
        spec = _native_spec(action, planned_kill=planned_kill)
        return env, spec, LiveBackend(cast(Any, env), spec)

    def test_fault_unwind_keeps_resources_until_cleanup_join(self) -> None:
        """Fault unwind releases parked calls without closing or killing their resources."""
        _env, _spec, backend = self._reader()
        read_fd, write_fd = os.pipe()
        self.addCleanup(os.close, write_fd)
        self.addCleanup(lambda: os.close(read_fd) if backend._executor.fds else None)
        process = mock.Mock()
        backend._executor.fds["held"] = read_fd
        backend._readers["r"] = process
        with (
            mock.patch.object(
                backend._faults, "unwind", return_value={"leaks": 0}
            ) as unwind,
            mock.patch(
                "eden.integration.correctness.exploratory.edenfs.backend_processes._kill_process_group"
            ) as kill,
        ):
            result = backend.unwind_faults(InMemorySink(), time.monotonic)
            unwind.assert_called_once()
            kill.assert_not_called()
            self.assertEqual(result["leaks"], 0)
            self.assertFalse(backend._readers_closed)
            self.assertEqual(backend._executor.fds, {"held": read_fd})
            self.assertIsNotNone(os.fstat(read_fd))
            process.poll.return_value = 0
            self.assertEqual(backend.close_resources(time.monotonic()), 0)
            self.assertTrue(backend._readers_closed)
            self.assertEqual(backend._executor.fds, {})
            with self.assertRaises(OSError):
                os.fstat(read_fd)

    def test_resource_close_sends_kill_without_an_extra_join(self) -> None:
        """A wedged reader receives SIGKILL without a fresh process wait budget."""
        _env, _spec, backend = self._reader()
        process = mock.Mock(pid=123456)
        process.poll.return_value = None
        backend._readers["r"] = process
        with mock.patch("os.killpg") as kill:
            self.assertEqual(backend.close_resources(time.monotonic()), 0)
            kill.assert_called_once_with(123456, signal.SIGKILL)
            process.wait.assert_not_called()
        self.assertTrue(backend._readers_closed)

    def test_background_read_finishes_real_sweep(self) -> None:
        env, spec, backend = self._reader()
        started = time.monotonic()
        result = backend(spec.action("r"))
        self.assertEqual(OK, result.outcome)
        self.assertGreaterEqual(time.monotonic() - started, 0.1)
        self.assertGreater(result.facts["sweeps"], 0)
        self.assertIsNone(env.processes.lookup("r"))
        with self.assertRaises(ProcessLookupError):
            os.killpg(result.facts["pgid"], 0)

    def _stop_after_first_sweep(self) -> Any:
        """Patch readers to SIGSTOP themselves after their first sweep.

        The stop follows the reader's parent-death setup and precedes its
        lease check, and spawning returns only once the reader has stopped.
        """
        sweep = "    print('sweep', flush=True)\n"
        program = backend_processes._BACKGROUND_READ
        self.assertIn(sweep, program)
        stopping = program.replace(
            sweep, f"{sweep}    os.kill(os.getpid(), {int(signal.SIGSTOP)})\n"
        )
        spawn = backend_processes.spawn

        def spawn_then_wait_stopped(
            registry: Any, key: str, argv: list[str], **popen: Any
        ) -> subprocess.Popen[str]:
            process = spawn(registry, key, [*argv[:2], stopping, *argv[3:]], **popen)
            self.addCleanup(_reap, process)
            state = os.waitid(
                os.P_PID, process.pid, os.WSTOPPED | os.WEXITED | os.WNOWAIT
            )
            assert state is not None
            self.assertEqual(os.CLD_STOPPED, state.si_code)
            return process

        return mock.patch.object(backend_processes, "spawn", spawn_then_wait_stopped)

    def _stopped_overrun(
        self, planned_kill: bool = True
    ) -> tuple[Any, LiveBackend, int, BackendResult]:
        """Return a lease whose stopped reader outlived it, still stopped."""
        env, spec, backend = self._reader(planned_kill=planned_kill)
        results: list[BackendResult] = []
        worker = threading.Thread(
            target=lambda: results.append(backend(spec.action("r")))
        )
        with self._stop_after_first_sweep():
            worker.start()
            worker.join(timeout=_HANG_GUARD)
        self.assertFalse(worker.is_alive(), "the lease waited for a stopped reader")
        pgid = env.processes.lookup("r")
        assert pgid is not None
        return env, backend, pgid, results[0]

    def test_lease_before_its_release_completes_info_and_the_release_runs(
        self,
    ) -> None:
        """`a0: r, x`, where only `x` lets the stopped reader run again.

        The overrun lease completes `info` without exit facts, `x` runs, the
        reader outlives its action's thread and finishes its walk, and unwind
        reaps it and records its late exit.
        """
        env, spec, _backend = self._reader()
        lease = spec.action("r")
        release = Action("x", "sleep", "operation", (("r", FINISHED),), {"seconds": 0})
        spec = replace(
            spec,
            budgets=Budgets(20.0, 30.0, 120.0),
            actors=(Actor("a0", ("r", "x")),),
            actions=(lease, release),
        )
        backend = LiveBackend(cast(Any, env), spec)

        def run(action: Action) -> BackendResult:
            if action.id != release.id:
                return backend(action)
            pgid = env.processes.lookup(lease.id)
            assert pgid is not None
            os.killpg(pgid, signal.SIGCONT)
            return BackendResult(OK, {})

        sink = InMemorySink()
        unwound: list[dict[str, Any]] = []
        with (
            self._stop_after_first_sweep(),
            mock.patch.object(backend._faults, "unwind", return_value={"leaks": 0}),
        ):
            status = run_actions(
                spec,
                run,
                sink,
                time.monotonic,
                unwind=lambda: unwound.append(
                    backend.unwind_faults(sink, time.monotonic)
                ),
            )
        ends = {e.data["action"]: e for e in sink.events if e.type in {OK, INFO}}
        self.assertEqual(COMPLETED, status.status, status)
        self.assertEqual((INFO, OK), (ends["r"].type, ends["x"].type))
        facts = {
            k: v for k, v in ends["r"].data.items() if k not in {"action", "actor"}
        }
        self.assertEqual({"reason", "pgid"}, set(facts))
        self.assertEqual(backend_processes.LEASE_OVERRUN, facts["reason"])
        schema = RECORDING_TARGET.actions["background_read"].result
        decoded = decode_result(schema, INFO, facts)
        self.assertNotIsInstance(decoded, ResultProblem)
        (result,) = unwound
        self.assertEqual((0, {"r": 0}), (result["leaks"], result["lease_overruns"]))
        self.assertIsNone(env.processes.lookup("r"))
        self.assertEqual(({}, set()), (backend._readers, backend._overrun_readers))
        with self.assertRaises(ProcessLookupError):
            os.killpg(facts["pgid"], 0)

    def test_overrun_reader_still_running_after_release_is_a_leak(self) -> None:
        env, backend, pgid, _result = self._stopped_overrun()
        self.assertEqual(
            (1, {"r": None}, {}), backend_processes.drain_overrun_readers(backend)
        )
        self.assertEqual(pgid, env.processes.lookup("r"))
        self.assertEqual({"r"}, backend._overrun_readers)
        self.assertEqual("T", _state(pgid))

    def test_overrun_reader_late_sigkill_counts_unless_its_spec_kill_signalled_it(
        self,
    ) -> None:
        """Only a SIGKILL sent by an executed spec `kill_process` is excused.

        A planned kill that never ran, or that found the reader already
        killed by someone else, does not excuse the late exit.
        """
        for planned_kill, killer, failures in (
            (False, "external", 1),
            (True, "external", 1),
            (True, "external-then-spec", 1),
            (True, "spec", 0),
        ):
            with self.subTest(planned_kill=planned_kill, killer=killer):
                env, backend, pgid, _result = self._stopped_overrun(planned_kill)
                if killer.startswith("external"):
                    os.killpg(pgid, signal.SIGKILL)
                    os.waitid(os.P_PID, pgid, os.WEXITED | os.WNOWAIT)
                if killer != "external":
                    killed = backend(backend._spec.action("k1"))
                    self.assertEqual(
                        killer == "external-then-spec",
                        killed.facts.get("already_exited", False),
                    )
                self.assertEqual(
                    (failures, {"r": -signal.SIGKILL}, {}),
                    backend_processes.drain_overrun_readers(backend),
                )
                self.assertIsNone(env.processes.lookup("r"))
                self.assertEqual({}, backend._signalled_readers)

    def test_overrun_reader_late_failure_keeps_its_stderr_tail(self) -> None:
        """A late positive exit fails cleanup even after a planned kill found it already exited."""
        code = "import sys; sys.stderr.write('x' * 5000 + 'late failure'); sys.exit(1)"
        for planned_kill in (False, True):
            with self.subTest(planned_kill=planned_kill):
                env, spec, backend = self._reader(planned_kill=planned_kill)
                process = backend_processes.spawn(
                    env.processes,
                    "r",
                    [sys.executable, "-c", code],
                    env=_runner_env(),
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                self.addCleanup(_reap, process)
                backend._readers["r"] = process
                backend._overrun_readers.add("r")
                if planned_kill:
                    os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOWAIT)
                    killed = backend(spec.action("k1"))
                    self.assertTrue(killed.facts["already_exited"], killed)
                with mock.patch.object(
                    backend._faults, "unwind", return_value={"leaks": 0}
                ):
                    result = backend.unwind_faults(InMemorySink(), time.monotonic)
                self.assertEqual(
                    (1, {"r": 1}), (result["leaks"], result["lease_overruns"])
                )
                tail = result["lease_overrun_stderr"]["r"]
                self.assertEqual(2000, len(tail))
                self.assertTrue(tail.endswith("late failure"), tail[-40:])

    def test_overrun_reader_exited_at_the_deadline_keeps_its_stderr_tail(self) -> None:
        env, _spec, backend = self._reader(planned_kill=False)
        process = backend_processes.spawn(
            env.processes,
            "r",
            [sys.executable, "-c", "import sys; sys.stderr.write('boom'); sys.exit(3)"],
            env=_runner_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(_reap, process)
        backend._readers["r"] = process
        backend._overrun_readers.add("r")
        os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOWAIT)
        with mock.patch.object(backend_processes, "_LEASE_DRAIN_SECONDS", 0.0):
            drained = backend_processes.drain_overrun_readers(backend)
        self.assertEqual((1, {"r": 3}, {"r": "boom"}), drained)

    def test_overrun_readers_share_one_drain_allowance(self) -> None:
        """Running readers share one process allowance; exited readers' pipes share one more."""
        env, _spec, backend = self._reader()
        now = [0.0]
        timeouts: list[float] = []

        def communicate(*steps: tuple[float, str | None]) -> Any:
            """Each call spends `seconds` (at most its timeout), then times out or returns `stderr`."""
            pending = list(steps)

            def call(timeout: float) -> tuple[str, str]:
                timeouts.append(timeout)
                seconds, stderr = pending.pop(0)
                now[0] += min(seconds, timeout)
                if stderr is None:
                    raise subprocess.TimeoutExpired("reader", timeout)
                return "", stderr

            return call

        wedged = (float("inf"), None)
        readers = {
            "a": (None, communicate(wedged)),
            "b": (None, communicate(wedged)),
            "c": (3, communicate(wedged, (0.5, "boom"))),
            "d": (0, communicate(wedged, wedged)),
        }
        for reader, (returncode, call) in readers.items():
            backend._readers[reader] = mock.Mock(
                communicate=call,
                poll=lambda code=returncode: code,
                returncode=returncode,
            )
            backend._overrun_readers.add(reader)
        with (
            mock.patch.object(backend_processes.time, "monotonic", lambda: now[0]),
            mock.patch.object(env.processes, "retire") as retire,
        ):
            drained = backend_processes.drain_overrun_readers(backend)
        exits = {"a": None, "b": None, "c": 3, "d": 0}
        self.assertEqual((3, exits, {"c": "boom"}), drained)
        drain = backend_processes._LEASE_DRAIN_SECONDS
        pipes = backend_processes._LATE_PIPE_SECONDS
        self.assertEqual([drain, 0.0, 0.0, pipes, 0.0, pipes - 0.5], timeouts)
        self.assertEqual(2, retire.call_count)
        self.assertEqual({"a", "b"}, backend._overrun_readers)

    def test_background_read_kill_and_unwind_empty_owned_group(self) -> None:
        for mode in ("kill", "unwind", "timeout"):
            with self.subTest(mode=mode):
                env, spec, backend = self._reader(3600.0)
                results = []
                worker = threading.Thread(
                    target=lambda backend=backend,
                    spec=spec,
                    results=results: results.append(backend(spec.action("r")))
                )
                worker.start()
                owned = env.processes.wait("r", _HANG_GUARD)
                try:
                    self.assertIsNotNone(owned)
                    assert owned is not None
                    if mode == "kill":
                        result = backend(spec.action("k1"))
                        self.assertEqual(owned.pgid, result.facts["pgid"])
                        self.assertTrue(result.facts["group_empty"])
                    else:
                        if mode == "timeout":
                            # Admission has stopped while this earlier action is still running.
                            timeout = replace(
                                spec,
                                budgets=Budgets(0.01, 0.1, 1.0),
                                actors=(Actor("a", ("z",)),),
                                actions=(
                                    Action(
                                        "z",
                                        "sleep",
                                        "operation",
                                        payload={"seconds": 0.1},
                                    ),
                                ),
                            )

                            def unwind(backend: LiveBackend = backend) -> None:
                                backend.unwind(InMemorySink(), time.monotonic)

                            run_actions(
                                timeout,
                                backend,
                                InMemorySink(),
                                time.monotonic,
                                unwind=unwind,
                            )
                        else:
                            backend.unwind(InMemorySink(), time.monotonic)
                    worker.join(timeout=_HANG_GUARD)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(FAIL, results[0].outcome)
                    self.assertIsNone(env.processes.lookup("r"))
                    with self.assertRaises(ProcessLookupError):
                        os.killpg(owned.pgid, 0)
                finally:
                    if owned is not None and owned.process.poll() is None:
                        owned.process.kill()
                        owned.process.wait(timeout=_HANG_GUARD)
                    worker.join(timeout=_HANG_GUARD)

    def test_background_read_dies_with_its_runner(self) -> None:
        env, spec, _backend = self._reader(3600.0)
        code = """
import sys, os, subprocess, threading
from pathlib import Path
from types import SimpleNamespace
from eden.integration.correctness.exploratory.core.target import ProcessRegistry
from eden.integration.correctness.exploratory.core.runspec import from_json
from eden.integration.correctness.exploratory.edenfs.target import RECORDING_TARGET
from eden.integration.correctness.exploratory.edenfs.backends import LiveBackend
spec = from_json(sys.argv[1], RECORDING_TARGET)
env = SimpleNamespace(mount_path=Path(sys.argv[2]), processes=ProcessRegistry(), setup_facts={})
backend = LiveBackend(env, spec)
ready = threading.Event()
original = subprocess.Popen
def spawn(*args, **kwargs):
    process = original(*args, **kwargs)
    communicate = process.communicate
    def receive(timeout=None):
        first = process.stdout.readline()
        assert first == 'sweep\\n'
        ready.set()
        output, error = communicate(timeout=timeout)
        return first + output, error
    process.communicate = receive
    return process
subprocess.Popen = spawn
threading.Thread(target=lambda: backend(spec.action('r')), daemon=True).start()
owned = env.processes.wait('r', 60.0)
assert owned is not None
assert ready.wait(60.0), 'reader never completed a sweep'
print(owned.pgid, flush=True)
os._exit(0)
"""
        result = subprocess.run(
            [sys.executable, "-c", code, to_json(spec), str(env.mount_path)],
            env=_runner_env(),
            capture_output=True,
            text=True,
            timeout=2 * _HANG_GUARD,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        pgid = int(result.stdout)
        deadline = time.monotonic() + _HANG_GUARD
        try:
            while time.monotonic() < deadline:
                try:
                    os.killpg(pgid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("reader group survived its runner")
        finally:
            try:
                os.killpg(pgid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def test_sleep_dispatch_returns_ok(self) -> None:
        action = Action(
            id="z1",
            kind="sleep",
            deadline=DEADLINE_OPERATION,
            payload={"seconds": 0},
        )

        result = LiveBackend(_NativeEnv(), _native_spec(action))(action)

        self.assertEqual(result, BackendResult(OK))

    def test_status_counts_reports_refusal_as_fact(self) -> None:
        """An sl status refusal remains an OK observation with its first line."""
        action = Action(
            id="o1",
            kind="status_counts",
            deadline=DEADLINE_OPERATION,
            payload={},
        )
        spec = _native_spec(action)
        env = _NativeEnv()
        sl = _CannedSl("X\nmore", returncode=255)
        with mock.patch.object(subprocess, "Popen", side_effect=sl) as popen:
            result = LiveBackend(env, spec)(action)
        self.assertEqual(result.outcome, OK)
        self.assertEqual(result.facts, {"refused": "X"})
        (command,), kwargs = popen.call_args
        self.assertEqual(["/bin/sl", "--traceback", "status", "--print0"], command)
        self.assertTrue(kwargs["start_new_session"])
        self.assertIsNone(env.processes.lookup("o1/status"))

    def test_status_publishes_exact_process_until_diff(self) -> None:
        action = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(action)
        env = _NativeEnv()
        sl = _CannedSl(f"M {REGISTERS[0]}\0", blocked=True)
        diff_saw: list[int | None] = []

        def popen_sl(argv: list[str], **kwargs: Any) -> subprocess.Popen[Any]:
            if "diff" in argv:
                diff_saw.append(env.processes.lookup(action.id))
            return sl(argv, **kwargs)

        results: list[Any] = []
        started = time.monotonic()
        with mock.patch.object(subprocess, "Popen", side_effect=popen_sl) as popen:
            worker = threading.Thread(
                target=lambda: results.append(LiveBackend(env, spec)(action))
            )
            worker.start()
            self.assertTrue(sl.entered.wait(timeout=_HANG_GUARD))
            owned = env.processes.wait(action.id, _HANG_GUARD)
            self.assertIsNotNone(owned)
            assert owned is not None
            process = sl.processes[0]
            self.assertIs(owned.process, process)
            self.assertEqual(owned.pgid, process.pid)
            self.assertGreaterEqual(owned.spawned_at, started)
            self.assertLessEqual(owned.spawned_at, time.monotonic())
            sl.release()
            worker.join(timeout=_HANG_GUARD)
        self.assertFalse(worker.is_alive())
        self.assertEqual(results[0].outcome, OK)
        self.assertEqual(
            [{"path": REGISTERS[0], "code": "M"}], results[0].facts["status"]
        )
        self.assertIsNone(env.processes.lookup(action.id))
        self.assertEqual([None], diff_saw)
        self.assertEqual(
            [call.args[0] for call in popen.call_args_list],
            [
                ["/bin/sl", "--traceback", "status", "--print0"],
                ["/bin/sl", "--traceback", "diff", "--stat", "tree"],
            ],
        )
        self.assertTrue(popen.call_args.kwargs["start_new_session"])

    def test_own_session_sl_descendant_is_killed_and_fails_the_run(self) -> None:
        """Live sl runs follow the reference side's rule: a setsid straggler is found by token."""
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        sl = Path(directory.name) / "sl"
        sl.write_text("#!/bin/sh\nsetsid sleep 3600 >/dev/null 2>&1 </dev/null &\n")
        sl.chmod(0o755)
        action = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        env = _NativeEnv()
        mount_repo = cast(Any, env.mount_repo)
        mount_repo.hg_bin = str(sl)
        mount_repo.path = directory.name
        mount_repo.hg_environment = dict(os.environ)
        backend = LiveBackend(env, _native_spec(action))
        with self.assertRaises(HarnessFailure) as context:
            backend(action)
        self.assertEqual(INVALID_BACKEND_RESPONSE, context.exception.reason)
        self.assertEqual([], backend._sl.leftovers())
        self.assertIsNone(env.processes.lookup("s1"))

    def test_checkout_update_and_go_use_native_processes(self) -> None:
        for via in ("goto",):
            with self.subTest(via=via):
                action = Action(
                    id="c1",
                    kind="checkout",
                    deadline=DEADLINE_OPERATION,
                    payload={"revision": 1, "via": via, "mode": "normal"},
                )
                spec = _native_spec(action)
                env = _NativeEnv()
                with mock.patch.object(
                    subprocess, "Popen", side_effect=_CannedSl("checkout output")
                ) as popen:
                    result = LiveBackend(env, spec)(action)
                self.assertEqual(result.outcome, OK)
                self.assertEqual(
                    popen.call_args.args[0], ["/bin/sl", "--traceback", via, "node1"]
                )
                self.assertIsNone(env.processes.lookup(action.id))

    def test_a_failed_diff_step_carries_no_exit_status(self) -> None:
        """`sl status` succeeds and `sl diff` exits 255 (a path removed while it
        reads): the failure has a reason and no `returncode`, so the contracts
        leave it untyped (`checkout._guard_refusal`)."""
        action = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        sl = _CannedSl(
            f"M {REGISTERS[0]}\0",
            "abort: No such file or directory: tree/registers/r1.reg",
            returncodes=(0, 255),
        )
        with mock.patch.object(subprocess, "Popen", side_effect=sl):
            result = LiveBackend(_NativeEnv(), _native_spec(action))(action)
        self.assertEqual(FAIL, result.outcome)
        self.assertEqual(
            {"reason": "diff: abort: No such file or directory: tree/registers/r1.reg"},
            result.facts,
        )

    def test_status_revision_uses_authored_node(self) -> None:
        action = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl", "revision": 1},
        )
        spec = _native_spec(action)
        env = _NativeEnv()
        with mock.patch.object(subprocess, "Popen", side_effect=_CannedSl()) as popen:
            result = LiveBackend(env, spec)(action)
        self.assertEqual(result.outcome, OK)
        self.assertEqual(
            popen.call_args_list[0].args[0],
            ["/bin/sl", "--traceback", "status", "--print0", "--rev", "node1"],
        )

    def test_nonzero_exit_is_raw_failure_independent_of_planned_kill(self) -> None:
        for returncode in (7, -signal.SIGKILL):
            for planned_kill in (False, True):
                with self.subTest(returncode=returncode, planned_kill=planned_kill):
                    action = Action(
                        id="c1",
                        kind="checkout",
                        deadline=DEADLINE_OPERATION,
                        payload={"revision": 1, "via": "goto", "mode": "normal"},
                    )
                    spec = _native_spec(action, planned_kill)
                    env = _NativeEnv()
                    sl = _CannedSl("actual checkout error", returncode=returncode)
                    with mock.patch.object(subprocess, "Popen", side_effect=sl):
                        result = LiveBackend(env, spec)(action)
                    self.assertEqual(result.outcome, FAIL)
                    self.assertEqual(result.facts["returncode"], returncode)
                    self.assertIn("actual checkout error", result.facts["reason"])

    def test_kill_process_targets_exact_group_and_waits_for_leader(self) -> None:
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        env = _NativeEnv()
        target_process: Any = _BlockingProcess("", pid=_session_leader(self))
        other_process: Any = _BlockingProcess("", pid=_session_leader(self))
        env.processes.publish("s1", target_process, time.monotonic())
        env.processes.publish("other", other_process, time.monotonic())
        kill = spec.action("k1")
        with mock.patch.object(
            os,
            "killpg",
            side_effect=[None, ProcessLookupError()],
        ) as killpg:
            result = LiveBackend(env, spec)(kill)
        self.assertEqual(result.outcome, OK)
        self.assertEqual(
            result.facts,
            {"target_action": "s1", "pgid": target_process.pid, "group_empty": True},
        )
        self.assertEqual(
            killpg.call_args_list[0].args, (target_process.pid, signal.SIGKILL)
        )
        self.assertTrue(target_process.waited)
        self.assertFalse(other_process.waited)

    def test_kill_process_of_an_sl_run_never_reaps_its_leader(self) -> None:
        """A planned kill of a running sl leaves the leader's one reap to its run.

        The kill reads the leader's exit through its pidfd, so ``poll()`` is
        never called on the Sapling ``Popen`` while the run still signals its
        group. Twins: a finished sl process reads as already exited, and a key
        no sl process used is not this owner's.
        """
        action = Action(
            id="c1",
            kind="checkout",
            deadline=DEADLINE_OPERATION,
            payload={"revision": 1, "via": "goto", "mode": "normal"},
        )
        spec = _native_spec(action, planned_kill=True)
        env = _NativeEnv()
        backend = LiveBackend(env, spec)
        sl = _CannedSl(blocked=True)
        self.addCleanup(sl.release)
        results: list[BackendResult] = []
        real_poll = _REAL_POPEN.poll
        with (
            mock.patch.object(subprocess, "Popen", side_effect=sl),
            mock.patch.object(
                _REAL_POPEN, "poll", autospec=True, side_effect=real_poll
            ) as poll,
        ):
            run = threading.Thread(target=lambda: results.append(backend(action)))
            run.start()
            self.assertTrue(sl.entered.wait(_HANG_GUARD))
            killed = backend(spec.action("k1"))
            run.join(_HANG_GUARD)
        self.assertEqual(
            (killed.outcome, killed.facts),
            (
                OK,
                {
                    "target_action": "c1",
                    "pgid": sl.processes[0].pid,
                    "group_empty": True,
                },
            ),
        )
        self.assertNotIn(
            sl.processes[0], [call.args[0] for call in poll.call_args_list]
        )
        self.assertEqual([FAIL], [result.outcome for result in results])
        self.assertEqual(-signal.SIGKILL, sl.processes[0].returncode)
        self.assertIsNone(env.processes.lookup("c1"))
        self.assertTrue(backend._sl.kill("c1", _HANG_GUARD))
        self.assertIsNone(backend._sl.kill("never-spawned", _HANG_GUARD))

    def test_kill_process_target_that_already_exited_is_ok(self) -> None:
        """A target that exited before the kill is ok with ``already_exited``."""
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        env = _NativeEnv()
        process = subprocess.Popen([sys.executable, "-c", ""], start_new_session=True)
        env.processes.publish("s1", process, time.monotonic())
        process.wait(timeout=_HANG_GUARD)
        result = LiveBackend(env, spec)(spec.action("k1"))
        self.assertEqual(result.outcome, OK)
        self.assertEqual(
            result.facts,
            {
                "target_action": "s1",
                "pgid": process.pid,
                "group_empty": True,
                "already_exited": True,
            },
        )

    def test_kill_process_target_its_owner_retired_is_not_signalled(self) -> None:
        """A target retired before the kill waits, or during its delay, is ``already_exited``.

        Its pgid may already belong to another group, so it is never
        signalled and its group is not checked (``group_empty: false``).
        """
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        for when in ("before-wait", "during-delay"):
            with self.subTest(when=when):
                env = _NativeEnv()
                process = subprocess.Popen(
                    [sys.executable, "-c", ""], start_new_session=True
                )
                env.processes.publish("s1", process, time.monotonic())
                process.wait(timeout=_HANG_GUARD)
                kill = spec.action("k1")

                def retire(*_args: Any, p: Any = process, e: Any = env) -> None:
                    e.processes.retire("s1", p)

                if when == "before-wait":
                    retire()
                else:
                    kill = replace(kill, payload={**kill.payload, "after_ms": 60_000})
                with (
                    mock.patch.object(os, "killpg") as killpg,
                    mock.patch.object(
                        backend_processes.time, "sleep", side_effect=retire
                    ),
                ):
                    result = LiveBackend(env, spec)(kill)
                self.assertEqual(
                    (
                        OK,
                        {
                            "target_action": "s1",
                            "pgid": process.pid,
                            "group_empty": False,
                            "already_exited": True,
                        },
                    ),
                    (result.outcome, result.facts),
                )
                killpg.assert_not_called()

    def test_kill_process_target_exiting_before_killpg_is_ok(self) -> None:
        """A target that exits between ``poll()`` and ``killpg`` is ok with ``already_exited``.

        killpg then raises ProcessLookupError; re-checking ``poll()`` shows
        the target exited, which is success. Twin: when ``poll()`` still
        reports it running, the missing group is a harness failure.
        """
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        for exits in (True, False):
            with self.subTest(exits=exits):
                env = _NativeEnv()
                process: Any = _BlockingProcess("", pid=_session_leader(self))
                process.poll = mock.Mock(side_effect=[None, 0 if exits else None])
                env.processes.publish("s1", process, time.monotonic())
                with mock.patch.object(os, "killpg", side_effect=ProcessLookupError()):
                    if not exits:
                        with self.assertRaises(HarnessFailure) as context:
                            LiveBackend(env, spec)(spec.action("k1"))
                        self.assertEqual(
                            context.exception.reason, INVALID_BACKEND_RESPONSE
                        )
                        continue
                    result = LiveBackend(env, spec)(spec.action("k1"))
                self.assertEqual(result.outcome, OK)
                self.assertEqual(
                    result.facts,
                    {
                        "target_action": "s1",
                        "pgid": process.pid,
                        "group_empty": True,
                        "already_exited": True,
                    },
                )

    def test_failed_close_after_write_records_close_errno(self) -> None:
        """A write whose close fails is a failed in-flight write with ``close_errno``.

        FUSE may report a deferred error at close, so the bytes may or may
        not have landed. A close failing after a failed pwrite keeps the
        pwrite's errno and adds ``close_errno``. Twin: a clean close is ok.
        """
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / REGISTERS[0]).parent.mkdir(parents=True)
        (root / REGISTERS[0]).write_bytes(b"")
        env = SimpleNamespace(mount_path=root, processes=ProcessRegistry())
        action = Action(
            "w1",
            "write",
            DEADLINE_OPERATION,
            payload={"path": REGISTERS[0], "value_id": "v1"},
        )
        backend = LiveBackend(cast(Any, env), _native_spec(action))
        real_close = os.close

        def failing_close(fd: int) -> None:
            real_close(fd)
            raise OSError(errno.EIO, "Input/output error")

        with mock.patch.object(os, "close", side_effect=failing_close):
            result = backend(action)
        self.assertEqual(result.outcome, FAIL)
        self.assertEqual(result.facts["errno"], errno.EIO)
        self.assertEqual(result.facts["close_errno"], errno.EIO)
        self.assertEqual(result.facts["bytes_written"], 64)
        self.assertTrue(result.facts["reason"].startswith("close: "))
        aborted = OSError(errno.ECONNABORTED, "Software caused connection abort")
        with (
            mock.patch.object(os, "pwrite", side_effect=aborted),
            mock.patch.object(os, "close", side_effect=failing_close),
        ):
            result = backend(action)
        self.assertEqual(result.outcome, FAIL)
        self.assertEqual(result.facts["errno"], errno.ECONNABORTED)
        self.assertEqual(result.facts["close_errno"], errno.EIO)
        self.assertTrue(result.facts["reason"].startswith("pwrite: "))
        result = backend(action)
        self.assertEqual((result.outcome, result.facts), (OK, {"bytes_written": 64}))

    def test_kill_process_uses_spawn_relative_delay(self) -> None:
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        kill = spec.action("k1")
        kill = Action(
            id=kill.id,
            kind=kill.kind,
            deadline=kill.deadline,
            requires=kill.requires,
            payload={"target_action": "s1", "after_ms": 500},
        )
        spec = _spec(REGISTERS[0], [target, kill], [("a", ["s1"]), ("nemesis", ["k1"])])
        env = _NativeEnv()
        process: Any = _BlockingProcess("", pid=_session_leader(self))
        env.processes.publish("s1", process, 100.0)
        slept: list[float] = []
        with (
            mock.patch(
                "eden.integration.correctness.exploratory.edenfs.backend_processes.time.monotonic",
                return_value=100.2,
            ),
            mock.patch(
                "eden.integration.correctness.exploratory.edenfs.backend_processes.time.sleep",
                side_effect=slept.append,
            ),
            mock.patch(
                "eden.integration.correctness.exploratory.edenfs.backend_processes._kill_process_group"
            ),
        ):
            LiveBackend(env, spec)(kill)
        self.assertEqual(len(slept), 1)
        self.assertAlmostEqual(slept[0], 0.3)

    def test_kill_process_of_a_target_never_spawned_is_harness_failure(self) -> None:
        """Twin of the retired-target test: with no process ever published, the kill fails."""
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        spec = RunSpec(
            spec.seed,
            spec.target,
            spec.environment,
            Budgets(30.0, 10.0, 0.01),
            spec.actors,
            spec.actions,
        )
        with self.assertRaises(HarnessFailure) as context:
            LiveBackend(_NativeEnv(), spec)(spec.action("k1"))
        self.assertEqual(context.exception.reason, INVALID_BACKEND_RESPONSE)

    def test_process_spawn_failure_is_harness_failure(self) -> None:
        action = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        with mock.patch.object(subprocess, "Popen", side_effect=OSError("no exec")):
            with self.assertRaises(HarnessFailure) as context:
                LiveBackend(_NativeEnv(), _native_spec(action))(action)
        self.assertEqual(context.exception.reason, INVALID_BACKEND_RESPONSE)

    def test_failed_write_keeps_its_errno(self) -> None:
        """A failed open or pwrite records its errno for effect contracts."""
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / REGISTERS[0]).parent.mkdir(parents=True)
        (root / REGISTERS[0]).write_bytes(b"")
        env = SimpleNamespace(mount_path=root, processes=ProcessRegistry())
        action = Action(
            "w1",
            "write",
            DEADLINE_OPERATION,
            payload={"path": REGISTERS[0], "value_id": "v1"},
        )
        backend = LiveBackend(cast(Any, env), _native_spec(action))
        aborted = OSError(errno.ECONNABORTED, "Software caused connection abort")
        with mock.patch.object(os, "pwrite", side_effect=aborted):
            result = backend(action)
        self.assertEqual(result.outcome, FAIL)
        self.assertEqual(result.facts["errno"], errno.ECONNABORTED)
        self.assertTrue(result.facts["reason"].startswith("pwrite: "))
        (root / REGISTERS[0]).unlink()
        result = backend(action)
        self.assertEqual(result.outcome, FAIL)
        self.assertEqual(result.facts["errno"], errno.ENOENT)
        self.assertTrue(result.facts["reason"].startswith("open: "))

    def test_kill_process_group_leak_is_harness_failure(self) -> None:
        target = Action(
            id="s1",
            kind="status",
            deadline=DEADLINE_OPERATION,
            payload={"channel": "sl"},
        )
        spec = _native_spec(target, planned_kill=True)
        env = _NativeEnv()
        process: Any = _BlockingProcess("", pid=_session_leader(self))
        env.processes.publish("s1", process, 0.0)
        with (
            mock.patch.object(os, "killpg", return_value=None),
            mock.patch(
                "eden.integration.correctness.exploratory.edenfs.backend_processes.time.monotonic",
                side_effect=[0.0, 0.0, 0.0, 0.0, 31.0],
            ),
        ):
            with self.assertRaises(HarnessFailure) as context:
                LiveBackend(env, spec)(spec.action("k1"))
        self.assertEqual(context.exception.reason, INVALID_BACKEND_RESPONSE)

    def test_kill_process_group_removes_wrapper_and_child(self) -> None:
        wrapper = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import subprocess,sys,time; "
                "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(3600)']); "
                "print(p.pid,flush=True); time.sleep(3600)",
            ],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self.addCleanup(wrapper.kill)
        assert wrapper.stdout is not None
        child_pid = int(wrapper.stdout.readline())
        _kill_process_group(wrapper, wrapper.pid)
        with self.assertRaises(ProcessLookupError):
            os.killpg(wrapper.pid, 0)
        for pid in (wrapper.pid, child_pid):
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)


class _ExitedStream:
    """Popen stub for a stream child that died before writing anything."""

    def __init__(self, returncode: int, pid: int = 4501) -> None:
        self.pid = pid
        self.returncode = returncode
        self.stdout = io.StringIO("")

    def poll(self) -> int:
        return self.returncode

    def communicate(self, _input: str) -> tuple[str, str]:
        return "", "died"


class ReaddirStreamBackendTest(unittest.TestCase):
    def _stream(
        self, entries: int, planned_kill: bool = False
    ) -> tuple[Any, RunSpec, LiveBackend]:
        """Back a stream over a local directory of long-named entries."""
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "tree").mkdir()
        for index in range(entries):
            (root / "tree" / f"{index:06d}-{'x' * 70}").write_bytes(b"")
        env = SimpleNamespace(
            mount_path=root, processes=ProcessRegistry(), setup_facts={}
        )
        stream = Action(
            "d0",
            "readdir_stream",
            "operation",
            payload={"path": "tree", "pause_after": max(1, entries // 2)},
        )
        resume = Action("d1", "readdir_resume", "operation", payload={"stream": "d0"})
        actions = [stream, resume]
        actors = [("a", ["d0", "d1"])]
        if planned_kill:
            kill = Action(
                "k1",
                "kill_process",
                "operation",
                requires=(("d0", STARTED),),
                payload={"target_action": "d0", "after_ms": 0},
            )
            actions.append(kill)
            actors.append(("nemesis", ["k1"]))
        spec = _spec(REGISTERS[0], actions, actors)
        return env, spec, LiveBackend(cast(Any, env), spec)

    def test_stream_records_getdents_position_at_pause_and_end(self) -> None:
        """A drain past the pause advances the directory fd past its paused position."""
        env, spec, backend = self._stream(2000)
        paused = backend(spec.action("d0"))
        self.assertEqual(OK, paused.outcome)
        self.assertEqual(1000, paused.facts["paused_at"])
        self.assertIsInstance(paused.facts["pos_at_pause"], int)
        drained = backend(spec.action("d1"))
        self.assertEqual(OK, drained.outcome)
        self.assertEqual(2000, len(set(drained.facts["entries"])))
        self.assertEqual(paused.facts["pos_at_pause"], drained.facts["pos_at_pause"])
        self.assertGreater(drained.facts["pos_at_end"], drained.facts["pos_at_pause"])
        self.assertIsNone(env.processes.lookup("d0"))

    def test_stream_without_a_getdents_straddle_is_harness_failure(self) -> None:
        """A directory prefetched whole by the pause proves nothing; the run is void."""
        env, spec, backend = self._stream(4)
        paused = backend(spec.action("d0"))
        self.assertEqual(OK, paused.outcome)
        with self.assertRaises(HarnessFailure) as context:
            backend(spec.action("d1"))
        self.assertEqual(INVALID_BACKEND_RESPONSE, context.exception.reason)
        self.assertIsNone(env.processes.lookup("d0"))
        with self.assertRaises(ProcessLookupError):
            os.killpg(paused.facts["pgid"], 0)

    def test_stream_death_before_pause_is_harness_failure_unless_killed(self) -> None:
        """A child gone before pausing is harness failure; a spec kill makes it FAIL."""
        for planned_kill in (False, True):
            with self.subTest(planned_kill=planned_kill):
                env, spec, backend = self._stream(4, planned_kill)
                with mock.patch.object(
                    subprocess,
                    "Popen",
                    return_value=_ExitedStream(1, pid=_session_leader(self)),
                ):
                    if planned_kill:
                        result = backend(spec.action("d0"))
                        self.assertEqual(FAIL, result.outcome)
                    else:
                        with self.assertRaises(HarnessFailure) as context:
                            backend(spec.action("d0"))
                        self.assertEqual(
                            INVALID_BACKEND_RESPONSE, context.exception.reason
                        )
                self.assertIsNone(env.processes.lookup("d0"))

    def test_stream_death_during_drain_is_harness_failure_unless_killed(self) -> None:
        """A child killed mid-drain is harness failure unless a spec kill targeted it."""
        for planned_kill in (False, True):
            with self.subTest(planned_kill=planned_kill):
                env, spec, backend = self._stream(2000, planned_kill)
                paused = backend(spec.action("d0"))
                self.assertEqual(OK, paused.outcome)
                os.killpg(paused.facts["pgid"], signal.SIGKILL)
                if planned_kill:
                    result = backend(spec.action("d1"))
                    self.assertEqual(FAIL, result.outcome)
                    self.assertEqual("stream exited -9", result.facts["reason"])
                else:
                    with self.assertRaises(HarnessFailure) as context:
                        backend(spec.action("d1"))
                    self.assertEqual(INVALID_BACKEND_RESPONSE, context.exception.reason)
                self.assertIsNone(env.processes.lookup("d0"))
