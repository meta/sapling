# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import base64
import importlib
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import AbstractContextManager
from unittest.mock import patch

from artillery.artillery2.api.cpp.cli_lite.python import art_cli_lite
from eden.fs.cli import main as edenfsctl
from facebook.contextprop.Baggage.thrift_types import Baggage
from facebook.contextprop.py3.ContextpropCore.thrift_types import (
    ArtilleryPropagatedContext,
)
from thrift.python.serializer import deserialize, Protocol

TRACE = "5ad4f2c7a3e14b8d1f2e3d4c5b6a7940"
AGENT = "AGENT:claude_code"
CERT = """\
-----BEGIN CERTIFICATE-----
MIIDSzCCAjOgAwIBAgIUBZH3MYYDAtH3rw4GEoicCwbFgyowDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNY2xpX2xpdGVfdGVzdDAgFw0yNjA5MDEwNTI2MDNaGA8y
MTI2MDgwODA1MjYwM1owGDEWMBQGA1UEAwwNY2xpX2xpdGVfdGVzdDCCASIwDQYJ
KoZIhvcNAQEBBQADggEPADCCAQoCggEBAKXL9A3z58/dnw9duMi1pjARHnDzMRRJ
1dFIGDMqJVN8O0ngpQdl2DM4dN1i32JArfIquwULAC1izthntgFo0uXNSNpRaclj
2oggdKXIwsWYKyK99q2dBi77kq5yVTLArBRVWrz1Q5/EMfyGDsxP/lsJ30Zy5tRo
KXACiJ/Vgo8pScQ/8cBKvNtUkcNQCUwRqNMfj0gLNZHtu1H5O6h0ps3OTZVGrpyl
zGO2QQAjt0dV1uxRcwtMhwfl11xjRo8vjFXCNmPtdsN00uUoyR0nfto1ZJ0zNCxB
SnC0fnO0+wIIjL/U77IYEtVRb1s8pWfYuL87D77VLKAG00hN9kAQ74sCAwEAAaOB
ijCBhzBmBgNVHREEXzBdhkFtaWQ6Ly9QUk9EL1VTRVIvMTIzND9mbGF2b3I9eCZh
Z2VudC5pZD1BR0VOVCUzYWNsYXVkZV9jb2RlJnRpZXI9eYYYbWlkOi8vUFJPRC9N
QUNISU5FL2hvc3QwMB0GA1UdDgQWBBTWPEQZLYG+v7eXfhis6aMrM+bJHTANBgkq
hkiG9w0BAQsFAAOCAQEApL49k77CDcILMCZd+aY2W70PG4OkbexS0ovBg5Zfok43
GMWkG612VKBcJOpymvs2sscvdVZNpb/szTNMBtlKsVaS9vpr5fGzm+34OH+dUZ7o
Apz+0yPCYeHLvErrmXf9zWZJ8usFzipWgqFcFNpeyQ/7p02p89/fVnKrzWIYarZl
/+6w9wq4XM0+SkapyRdW0Q+LqpvVIBQJUIOgY+pD1OMPJCIpuEwh5QJYc8C2Dae2
8c+HXC0sz0TCyWiriU3ErKL9BQObu/wrDYbRt8YWOryHp0Zvx7M8p9JxuOFoEgJE
ox+ZgOXJBgPJwkSmw7K5ugovFBuStJFUB5jaM2FYrQ==
-----END CERTIFICATE-----
"""
AMBIENT = (
    "TRACEPARENT",
    "CONTEXTPROP_BAGGAGE",
    "ARTILLERY_CONTEXT",
    "CLAUDE_CODE_CURRENT_SESSION_ID",
    "CC_TRACE_DIR",
    "CODING_AGENT_METADATA",
    "META_3PAI_INVOCATION_ID",
    "AGENT",
    "CONFUCIUS_IN_NAMESPACE",
    "ARTILLERY_KILLSWITCH_PATH",
    "THRIFT_TLS_CL_CERT_PATH",
)
CARRIERS = (
    "CONTEXTPROP_BAGGAGE",
    "ARTILLERY_CONTEXT",
    "TRACEPARENT",
    "ART_CLI_LITE_EXEC_PID",
)
DUMP = """import json, os, sys
json.dump(dict(os.environ), open(sys.argv[1], "w"))
print("helper stdout")
print("helper stderr", file=sys.stderr)
"""
SH_DUMP = """#!/bin/sh
echo helper stdout
echo helper stderr >&2
exec env -0 > "$1"
"""


def write_helper(directory: str) -> list[str]:
    if os.name == "nt":
        path = os.path.join(directory, "dump.py")
        with open(path, "w") as f:
            f.write(DUMP)
        return [sys.executable, path]
    path = os.path.join(directory, "dump.sh")
    with open(path, "w") as f:
        f.write(SH_DUMP)
    os.chmod(path, 0o755)
    return [path]


def read_env(path: str) -> dict[str, str]:
    with open(path, "rb") as f:
        data = f.read()
    if data.startswith(b"{"):
        return json.loads(data)
    return dict(entry.decode().split("=", 1) for entry in data.split(b"\0") if entry)


def decoded(value: str) -> bytes:
    value = value.replace("+", "-").replace("/", "_")
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


def agent_ids(child: dict[str, str]) -> tuple[str | None, str | None]:
    apc = deserialize(
        ArtilleryPropagatedContext,
        decoded(child["ARTILLERY_CONTEXT"]),
        protocol=Protocol.COMPACT,
    )
    cp2 = deserialize(
        Baggage, decoded(child["CONTEXTPROP_BAGGAGE"]), protocol=Protocol.COMPACT
    )
    context = cp2.trace_context
    assert context is not None
    tracing = context.tracing_context
    assert tracing is not None
    assert apc.trace_id == int.from_bytes(bytes.fromhex(TRACE[16:]), "little")
    assert (tracing.trace_id, tracing.edge_id) == (apc.trace_id, apc.edge_id)
    return apc.agent_id, cp2.agent_id


def wait_for(path: str, timeout: float = 60.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(path):
            return True
        time.sleep(0.01)
    return False


def without_carriers(env: dict[str, str]) -> dict[str, str]:
    return {k: v for k, v in env.items() if k not in CARRIERS}


class PythonEdenfsctlTracingTest(unittest.TestCase):
    def setUp(self) -> None:
        saved = {name: os.environ[name] for name in AMBIENT if name in os.environ}
        self.addCleanup(os.environ.update, saved)
        self.addCleanup(self.scrub)
        self.addCleanup(art_cli_lite.clear)
        self.scrub()
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.helper = write_helper(self.dir.name)

    def scrub(self) -> None:
        for name in AMBIENT:
            os.environ.pop(name, None)
            os.unsetenv(name)

    def agent_control(self, control: dict[str, object]) -> None:
        path = os.path.join(self.dir.name, "control.json")
        with open(path, "w") as f:
            json.dump({"transports": ["none"], "observe_sample_rate": 0, **control}, f)
        cert = os.path.join(self.dir.name, "cert.pem")
        with open(cert, "w") as f:
            f.write(CERT)
        os.environ.update(
            TRACEPARENT=f"00-{TRACE}-b7ad6b7169203331-01",
            CODING_AGENT_METADATA="id=claude_code",
            ARTILLERY_KILLSWITCH_PATH=path,
            THRIFT_TLS_CL_CERT_PATH=cert,
        )

    def run_helper(self) -> tuple[int, str, str, dict[str, str]]:
        out = os.path.join(self.dir.name, "env.json")
        done = subprocess.run(
            [*self.helper, out], env=dict(os.environ), capture_output=True, text=True
        )
        return done.returncode, done.stdout, done.stderr, read_env(out)

    def baseline_and_child(
        self, control: dict[str, object] | None
    ) -> tuple[
        object,
        tuple[int, str, str, dict[str, str]],
        tuple[int, str, str, dict[str, str]],
    ]:
        if control is not None:
            self.agent_control(control)
        bare = self.run_helper()
        with (
            patch.object(edenfsctl, "main", return_value=0),
            self.assertRaises(SystemExit) as exited,
        ):
            edenfsctl.zipapp_main()
        return exited.exception.code, bare, self.run_helper()

    def test_an_agent_runs_a_child_continuing_the_agents_trace(self) -> None:
        code, bare, child = self.baseline_and_child(
            {"killed": True, "overrides": {"edenfsctl": {"killed": False}}}
        )
        self.assertEqual(code, 0)
        self.assertEqual(child[:3], bare[:3])
        self.assertEqual(agent_ids(child[3]), (AGENT, AGENT))
        self.assertEqual(child[3]["TRACEPARENT"], "")
        self.assertEqual(without_carriers(child[3]), without_carriers(bare[3]))

    def assert_unchanged(self, control: dict[str, object] | None) -> None:
        code, bare, child = self.baseline_and_child(control)
        self.assertEqual(code, 0)
        self.assertEqual(child, bare)

    def test_a_human_runs_a_child_unchanged(self) -> None:
        self.assert_unchanged(None)

    def test_a_killed_edenfsctl_runs_an_agents_child_unchanged(self) -> None:
        self.assert_unchanged(
            {"killed": False, "overrides": {"edenfsctl": {"killed": True}}}
        )

    def test_a_sampled_out_edenfsctl_runs_an_agents_child_unchanged(self) -> None:
        self.assert_unchanged(
            {"killed": False, "overrides": {"edenfsctl": {"sample_rate": 0.0}}}
        )

    def hard_exit(self, fault: AbstractContextManager[object]) -> tuple[int, float]:
        self.agent_control(
            {"killed": True, "overrides": {"edenfsctl": {"killed": False}}}
        )
        started = time.monotonic()
        pid = os.fork()
        if pid == 0:
            try:
                with (
                    patch.object(sys, "argv", ["edenfsctl", "list"]),
                    patch.object(
                        edenfsctl, "asyncio_run", side_effect=KeyboardInterrupt
                    ),
                    fault,
                ):
                    edenfsctl.zipapp_main()
            finally:
                os._exit(99)
        _, status = os.waitpid(pid, 0)
        return os.waitstatus_to_exitcode(status), time.monotonic() - started

    @unittest.skipIf(os.name == "nt", "os.fork")
    def test_a_hung_block_shipment_exits_130_after_one_second(self) -> None:
        native = importlib.import_module("artillery.art_cli_lite.art_cli_lite_py")
        code, elapsed = self.hard_exit(
            patch.object(native, "finish_process", side_effect=lambda: time.sleep(30))
        )
        self.assertEqual(code, 130)
        self.assertGreaterEqual(elapsed, 1.0)
        self.assertLess(elapsed, 1.6)

    @unittest.skipIf(os.name == "nt", "os.fork")
    def test_a_finisher_that_cannot_start_still_exits_130(self) -> None:
        code, elapsed = self.hard_exit(
            patch.object(
                threading.Thread, "start", side_effect=RuntimeError("no thread")
            )
        )
        self.assertEqual(code, 130)
        self.assertLess(elapsed, 0.6)

    @unittest.skipIf(os.name == "nt", "os.fork")
    def test_a_second_interrupt_while_finishing_still_exits_130(self) -> None:
        code, elapsed = self.hard_exit(
            patch.object(threading.Thread, "join", side_effect=KeyboardInterrupt)
        )
        self.assertEqual(code, 130)
        self.assertLess(elapsed, 0.6)


class _Listener:
    def __init__(self, family: int, address: str | tuple[str, int]) -> None:
        self.server = socket.socket(family, socket.SOCK_STREAM)
        self.server.bind(address)
        self.server.listen(8)
        self.received = bytearray()
        self.first = threading.Event()
        self.held: list[socket.socket] = []
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self) -> None:
        while True:
            try:
                conn, _ = self.server.accept()
            except OSError:
                return
            self.held.append(conn)
            threading.Thread(target=self.read, args=(conn,), daemon=True).start()

    def read(self, conn: socket.socket) -> None:
        while chunk := conn.recv(65536):
            self.received += chunk
            self.first.set()

    def close(self) -> None:
        for conn in self.held:
            conn.close()
        self.server.close()


@unittest.skipIf(os.name == "nt", "Ctrl-C is delivered as SIGINT to the process group")
class InterruptedEdenfsctlTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="eden")
        self.scribed = _Listener(socket.AF_INET, ("127.0.0.1", 0))
        self.addCleanup(self.scribed.close)

    def environment(self, control: dict[str, object] | None) -> dict[str, str]:
        env = {
            k: v
            for k, v in os.environ.items()
            if k not in AMBIENT
            and not k.startswith(("FB_PAR_", "FB_XAR_", "PYTHON", "IMPORTS_MONITOR_"))
            and k != "THRIFT_TLS_CL_KEY_PATH"
        }
        cert = os.path.join(self.dir, "cert.pem")
        with open(cert, "w") as f:
            f.write(CERT)
        env.update(
            EDENFS_NO_TELEMETRY="1",
            SCRIBE_CLIENT_HOST="127.0.0.1",
            SCRIBE_CLIENT_PORT=str(self.scribed.server.getsockname()[1]),
            THRIFT_TLS_CL_CERT_PATH=cert,
        )
        if control is not None:
            path = os.path.join(self.dir, "control.json")
            with open(path, "w") as f:
                json.dump(
                    {
                        "transports": ["scribe_socket"],
                        "observe_sample_rate": 0,
                        **control,
                    },
                    f,
                )
            env.update(
                TRACEPARENT=f"00-{TRACE}-b7ad6b7169203331-01",
                CODING_AGENT_METADATA="id=claude_code",
                ARTILLERY_KILLSWITCH_PATH=path,
            )
        return env

    def interrupt(
        self, control: dict[str, object] | None, interrupts: int = 1
    ) -> tuple[int, float]:
        env = self.environment(control)
        parked = os.path.join(self.dir, "parked")
        env["INTERRUPT_PROBE_STARTED"] = parked
        child = subprocess.Popen(
            [os.environ["INTERRUPT_PROBE"], "ctrl_c"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.addCleanup(child.kill)
        self.assertTrue(wait_for(parked), "edenfsctl reached its command")
        interrupted = time.monotonic()
        os.killpg(child.pid, signal.SIGINT)
        for _ in range(interrupts - 1):
            self.assertTrue(
                self.scribed.first.wait(5), "edenfsctl is shipping the block"
            )
            os.killpg(child.pid, signal.SIGINT)
        stdout, stderr = child.communicate(timeout=30)
        self.assertEqual((stdout, stderr), (b"", b""), "Ctrl-C prints nothing")
        return child.returncode, time.monotonic() - interrupted

    def test_a_traced_ctrl_c_ships_the_block_and_exits_130_within_the_bound(
        self,
    ) -> None:
        code, elapsed = self.interrupt(
            {"killed": True, "overrides": {"edenfsctl": {"killed": False}}}
        )
        self.assertEqual(code, 130)
        self.assertTrue(self.scribed.first.wait(5), "the block reached scribed")
        self.assertIn(b"artillery_trace_events", self.scribed.received)
        self.assertIn(b"cli.edenfsctl", self.scribed.received)
        self.assertIn(AGENT.encode(), self.scribed.received)
        self.assertLess(elapsed, 1.0 + 0.5)

    def test_a_second_ctrl_c_while_finishing_still_exits_130(self) -> None:
        code, elapsed = self.interrupt(
            {"killed": True, "overrides": {"edenfsctl": {"killed": False}}},
            interrupts=2,
        )
        self.assertEqual(code, 130)
        self.assertLess(elapsed, 1.0 + 0.5)

    def entrypoint(
        self, control: dict[str, object] | None
    ) -> tuple[int, str, str, dict[str, str]]:
        daemon = os.path.join(self.dir, "daemon.sh")
        out = os.path.join(self.dir, "daemon.env")
        with open(daemon, "w") as f:
            f.write(
                f'#!/bin/sh\necho daemon stdout\necho daemon stderr >&2\nexec env -0 > "{out}"\n'
            )
        os.chmod(daemon, 0o755)
        config = tempfile.mkdtemp(prefix="eden")
        done = subprocess.run(
            [
                os.environ["EDENFSCTL_PY"],
                "--config-dir",
                config,
                "start",
                "--foreground",
                "--daemon-binary",
                daemon,
            ],
            env=self.environment(control),
            capture_output=True,
            text=True,
            timeout=120,
        )
        child = read_env(out)
        os.unlink(out)
        return done.returncode, done.stdout, done.stderr, child

    def test_the_real_entrypoint_matches_an_untraced_run(self) -> None:
        bare = self.entrypoint(None)
        self.assertEqual(bare[1:3], ("daemon stdout\n", "daemon stderr\n"))
        controls: tuple[dict[str, object], ...] = (
            {"killed": False, "overrides": {"edenfsctl": {"killed": True}}},
            {"killed": False, "overrides": {"edenfsctl": {"sample_rate": 0.0}}},
        )
        for control in controls:
            with self.subTest(control=control):
                self.assertEqual(self.entrypoint(control), bare)
        traced = self.entrypoint(
            {"killed": True, "overrides": {"edenfsctl": {"killed": False}}}
        )
        self.assertEqual(traced[:3], bare[:3])
        self.assertEqual(agent_ids(traced[3]), (AGENT, AGENT))
        self.assertEqual(without_carriers(traced[3]), without_carriers(bare[3]))

    def assert_nothing_shipped(self, control: dict[str, object] | None) -> None:
        code, elapsed = self.interrupt(control)
        self.assertEqual(code, 130)
        self.assertFalse(self.scribed.first.wait(1), "nothing reached scribed")
        self.assertLess(elapsed, 0.5)

    def test_a_human_ctrl_c_ships_nothing(self) -> None:
        self.assert_nothing_shipped(None)

    def test_a_killed_edenfsctl_ctrl_c_ships_nothing(self) -> None:
        self.assert_nothing_shipped(
            {"killed": False, "overrides": {"edenfsctl": {"killed": True}}}
        )

    def test_a_sampled_out_edenfsctl_ctrl_c_ships_nothing(self) -> None:
        self.assert_nothing_shipped(
            {"killed": False, "overrides": {"edenfsctl": {"sample_rate": 0.0}}}
        )
