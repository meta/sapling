# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Live file backends over a real EdenFS mount.

``LiveBackend`` implements the scheduler Backend on an Environment mount and
issues each action's syscalls, RPCs, or sl command once, with no retries.
Red controls are silent faults the checker must reject: torn-write writes
only the first 32 bytes, status-loss drops one M entry from status facts,
diff-loss drops one M entry from diff_paths, and common-status-loss drops
the same M entry from both.
"""

from __future__ import annotations

import os
import subprocess
import sys
import threading
import time
from functools import partial
from typing import Any, cast

from eden.fs.service.eden.thrift_types import (
    FileAttributeDataOrErrorV2,
    FileAttributes,
    GetAttributesFromFilesParams,
    GetFileContentRequest,
    MountId,
    ScmBlobOrError,
    SourceControlType,
    SourceControlTypeOrError,
    SyncBehavior,
)
from eden.integration.correctness.exploratory.core.checker import content_digest
from eden.integration.correctness.exploratory.core.history import FAIL, HistorySink, OK
from eden.integration.correctness.exploratory.core.processes import kill_group
from eden.integration.correctness.exploratory.core.runspec import Action, RunSpec
from eden.integration.correctness.exploratory.core.scheduler import (
    BackendResult,
    Clock,
    HarnessFailure,
    INVALID_BACKEND_RESPONSE,
)
from eden.integration.correctness.exploratory.edenfs import (
    backend_processes as processes,
    backend_scm as scm,
)
from eden.integration.correctness.exploratory.edenfs.backend_scm import (
    _DAEMON_RPC_FAILURES,
    RED_COMMON_STATUS_LOSS,
    RED_CONTROLS,
    RED_DIFF_LOSS,
    RED_STATUS_LOSS,
    RED_TORN_WRITE,
)
from eden.integration.correctness.exploratory.edenfs.encoding import encode_value
from eden.integration.correctness.exploratory.edenfs.environment import Environment
from eden.integration.correctness.exploratory.edenfs.executor import (
    serves,
    SharedExecutor,
)
from eden.integration.correctness.exploratory.edenfs.faults import FaultTracker
from eden.integration.correctness.exploratory.edenfs.sapling_processes import (
    SaplingProcesses,
)

__all__ = [
    "LiveBackend",
    "RED_COMMON_STATUS_LOSS",
    "RED_CONTROLS",
    "RED_DIFF_LOSS",
    "RED_STATUS_LOSS",
    "RED_TORN_WRITE",
    "TORN_BYTES",
]

TORN_BYTES = 32
_SCM_TYPES = {
    SourceControlType.REGULAR_FILE: "file",
    SourceControlType.EXECUTABLE_FILE: "file",
    SourceControlType.TREE: "directory",
    SourceControlType.SYMLINK: "symlink",
}


def _harness_failure(kind: str, detail: str) -> HarnessFailure:
    """An sl process without a trustworthy result fails the run, whatever its kind."""
    print(f"{kind}: {detail}", file=sys.stderr, flush=True)
    return HarnessFailure(INVALID_BACKEND_RESPONSE)


class LiveBackend:
    """Scheduler backend performing one real syscall or RPC per action."""

    def __init__(
        self,
        env: Environment,
        spec: RunSpec,
        red_control: str | None = None,
    ) -> None:
        """Bind to a set-up Environment; register bytes follow the spec seed."""
        if red_control is not None and red_control not in RED_CONTROLS:
            raise ValueError(f"unknown red_control {red_control!r}")
        self._env = env
        self._spec = spec
        self._red_control = red_control
        self._faults = FaultTracker(env, spec)
        self._reader_lock = threading.Lock()
        self._readers: dict[str, subprocess.Popen[str]] = {}
        self._overrun_readers: set[str] = set()
        # Readers an executed `kill_process` signalled, by pgid.
        self._signalled_readers: dict[str, int] = {}
        self._readers_closed = False
        self._sl = SaplingProcesses(env.processes, _harness_failure)
        self._executor = SharedExecutor(
            lambda path: str(self._env.mount_path / path),
            self._write_bytes,
            lambda revision: self._env.commits[revision].node,
            partial(scm.run_sl_process, self),
            _harness_failure,
        )

    def _expected_bytes(self, action: Action) -> bytes:
        value_id = action.payload["value_id"]
        return encode_value(self._spec.seed, self._spec.owner(action.id), value_id)

    def _write_bytes(self, action: Action) -> bytes:
        """Return the bytes one write puts; the torn-write control keeps the first 32."""
        payload = self._expected_bytes(action)
        if self._red_control == RED_TORN_WRITE:
            return payload[:TORN_BYTES]
        return payload

    def __call__(self, action: Action) -> BackendResult:
        """Run one action with a single syscall or RPC and no retries."""
        if action.kind == "fault_install":
            return self._faults.install(action)
        if action.kind == "fault_await":
            return self._faults.wait_for_hit(action)
        if action.kind == "fault_remove":
            return self._faults.remove(action)
        result = self._run_target(action)
        return self._faults.record_completion(action, result)

    def _run_target(self, action: Action) -> BackendResult:  # noqa: C901
        """Run one non-fault action before recording its concrete completion.

        Every action the reference side can run too goes through the shared
        executor; getattr, sl status, and sl checkout add live-only facts.
        """
        kind = action.kind
        if kind == "getattr":
            return self._getattr(action)
        if kind == "status" and action.payload["channel"] == "sl":
            return scm.status_sl(self, action)
        if kind == "checkout" and action.payload["via"] in {"goto"}:
            return scm.checkout(self, action)
        if serves(self._spec, action):
            return self._executor(action)
        if kind == "sleep":
            time.sleep(action.payload["seconds"])
            return BackendResult(OK)
        if kind == "background_read":
            return processes.background_read(self, action)
        if kind == "readdir_stream":
            return processes.readdir_stream(self, action)
        if kind == "readdir_resume":
            return processes.readdir_resume(self, action)
        if kind == "readdir":
            return scm.readdir_thrift(self, action.payload["path"])
        if kind == "unload_inodes":
            return scm.unload_inodes(self, action)
        if kind == "read":
            return self._read_thrift(action)
        return self._run_scm_target(action)

    def _run_scm_target(self, action: Action) -> BackendResult:
        """Run one Thrift checkout or status, journal, or nemesis action."""
        if action.kind == "checkout" and action.payload["via"] == "thrift":
            return scm.checkout_thrift(self, action)
        if action.kind == "status" and action.payload["channel"] == "thrift":
            return scm.status_thrift(self, action)
        if action.kind == "manifest_presence":
            return scm.manifest_presence(self, action)
        if action.kind == "journal_window":
            return scm.journal_window(self, action)
        if action.kind == "status_counts":
            return scm.status_counts(self, action)
        if action.kind == "kill_process":
            return processes.kill_process(self, action)
        raise HarnessFailure(INVALID_BACKEND_RESPONSE)

    def unwind_faults(self, sink: HistorySink, clock: Clock) -> dict[str, Any]:
        """Release faults before workers join, retaining their descriptors and processes.

        Only overrun leases' readers are reaped here (`drain_overrun_readers`);
        their exits are `lease_overruns`, the stderr tails of nonzero exits
        `lease_overrun_stderr`, and their failures are leaks.
        """
        result: dict[str, Any] = dict(self._faults.unwind(sink, clock))
        failures, exits, stderr_tails = processes.drain_overrun_readers(self)
        result["leaks"] += failures
        if exits:
            result["lease_overruns"] = exits
        if stderr_tails:
            result["lease_overrun_stderr"] = stderr_tails
        result["setup"] = dict(self._env.setup_facts)
        return result

    def close_resources(self, deadline: float | None = None) -> int:
        """Close backend resources after joining; containment permits no extra process wait."""
        with self._reader_lock:
            self._readers_closed = True
            readers = tuple(self._readers.values())
            self._signalled_readers.clear()
        self._executor.close()
        leaks = 0
        for process in readers:
            try:
                if process.poll() is None:
                    if deadline is None:
                        processes._kill_process_group(process, process.pid)
                    else:
                        kill_group(process.pid)
            except HarnessFailure:
                leaks += int(process.poll() is None)
        return leaks + self._sl.close_resources(deadline)

    def unwind(self, sink: HistorySink, clock: Clock) -> dict[str, Any]:
        """Release faults and resources for callers without staged containment cleanup."""
        result = self.unwind_faults(sink, clock)
        result["leaks"] += self.close_resources()
        return result

    def _read_thrift(self, action: Action) -> BackendResult:
        """Fetch the register via one getFileContent RPC (executor shape)."""
        register = action.payload["path"]
        sync_timeout_seconds = action.payload["sync_timeout_seconds"]
        request = GetFileContentRequest(
            mount=MountId(mountPoint=os.fsencode(self._env.mount_path)),
            filePath=os.fsencode(register),
            sync=SyncBehavior(syncTimeoutSeconds=sync_timeout_seconds),
        )
        try:
            with self._env.get_thrift_client() as client:
                response = cast(Any, client).getFileContent(request)
        except _DAEMON_RPC_FAILURES as error:
            return BackendResult(FAIL, {"reason": f"rpc: {error}"})
        if response.blob.get_type() == ScmBlobOrError.Type.blob:
            blob = bytes(response.blob.blob)
            if not blob:
                return BackendResult(FAIL, {"reason": "empty read"})
            return BackendResult(OK, content_digest(blob))
        error = response.blob.error
        facts = {
            "reason": f"thrift: {error.message}",
            "source": "thrift",
            "error_type": error.errorType.name,
            "message": error.message,
        }
        if error.errorCode is not None:
            facts["error_code"] = error.errorCode
        return BackendResult(FAIL, facts)

    def _getattr(self, action: Action) -> BackendResult:
        """Stat the path through the shared executor, then add its Thrift source-control type."""
        result = self._executor(action)
        if result.outcome != OK:
            return result
        return BackendResult(
            OK, {**result.facts, **self._thrift_type(action.payload["path"])}
        )

    def _thrift_type(self, register: str) -> dict[str, Any]:
        """Return the path's source-control type as seen by one attributes RPC."""
        request = GetAttributesFromFilesParams(
            mountPoint=os.fsencode(self._env.mount_path),
            paths=[os.fsencode(register)],
            requestedAttributes=int(FileAttributes.SOURCE_CONTROL_TYPE),
            sync=SyncBehavior(syncTimeoutSeconds=0),
        )
        try:
            with self._env.get_thrift_client() as client:
                (entry,) = cast(Any, client).getAttributesFromFilesV2(request).res
        except Exception as error:  # noqa: BLE001 - recorded, one channel of two
            return {"thrift_error": f"rpc: {error}"}
        if entry.get_type() != FileAttributeDataOrErrorV2.Type.fileAttributeData:
            return {"thrift_error": entry.error.message}
        kind = entry.fileAttributeData.sourceControlType
        if kind.get_type() != SourceControlTypeOrError.Type.sourceControlType:
            return {"thrift_error": kind.error.message}
        return {"thrift_type": _SCM_TYPES.get(kind.sourceControlType, "other")}
