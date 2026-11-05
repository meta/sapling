#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import json
import logging
import os
import pathlib
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import contextmanager, ExitStack
from pathlib import Path
from types import TracebackType
from typing import (
    Any,
    BinaryIO,
    cast,
    Dict,
    Generator,
    Iterable,
    List,
    Optional,
    Sequence,
    TextIO,
    Tuple,
    Union,
)

from eden.fs.cli import mtab, proc_utils as proc_utils_mod, util
from eden.fs.service.eden.thrift_clients import EdenService
from eden.fs.service.eden.thrift_types import MountInfo, MountState
from eden.thrift import client
from fb303_core.thrift_types import fb303_status

from .find_executables import FindExe

# Two minutes is very generous, but 30 seconds is not enough CI hosts
# and many-core machines under load.
EDENFS_START_TIMEOUT = 120
EDENFS_STOP_TIMEOUT = 240


def require_io_uring_kernel() -> None:
    release = os.uname().release if sys.platform == "linux" else ""
    # The FUSE io_uring ABI is kernel-specific; extend this allowlist
    # when another fbk release is validated.
    if "fbk" not in release or not release.startswith(("6.13.", "6.16.")):
        raise unittest.SkipTest("requires an fbk 6.13 or 6.16 FUSE io_uring kernel")


def fuse_transport_config(use_io_uring: bool) -> list[str]:
    settings = [f"use-io-uring = {'true' if use_io_uring else 'false'}"]
    if use_io_uring:
        settings.append('io-uring-kernel-release-regex = ".*"')
        # Allocate queues before replying to INIT to allow devfuse fallback.
        settings.append("io-uring-pre-create-queues = true")
    return settings


def assert_fuse_transport(
    mount_point: bytes, expected: str, actual: str | None
) -> None:
    if actual == expected:
        return
    message = (
        f"FUSE transport for {os.fsdecode(mount_point)}: "
        f"expected {expected!r}, got {actual!r}"
    )
    if expected == "io_uring" and actual == "devfuse":
        raise unittest.SkipTest(
            f"{message}; io_uring unavailable, using devfuse fallback"
        )
    raise AssertionError(message)


def assert_fuse_transports(mounts: Iterable[MountInfo], expected: str) -> None:
    fallbacks = []
    for mount in mounts:
        if mount.state != MountState.RUNNING or mount.fsChannelType != "fuse":
            continue
        try:
            assert_fuse_transport(mount.mountPoint, expected, mount.fuseTransport)
        except unittest.SkipTest as ex:
            fallbacks.append(str(ex))
    if fallbacks:
        raise unittest.SkipTest("; ".join(fallbacks))


def is_process_running(pid: Optional[int]) -> bool:
    if pid is None:
        return False

    if sys.platform.startswith("linux"):
        try:
            stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
            state = stat.rsplit(")", 1)[1].strip().split()[0]
            if state == "Z":
                return False
        except FileNotFoundError:
            return False
        except (IndexError, OSError):
            pass

    return proc_utils_mod.new().is_process_alive(pid)


class EdenFS:
    """Manages an instance of the EdenFS fuse server."""

    _eden_dir: Path
    start_time: float = 0.0
    last_event: float = 0.0

    def __init__(
        self,
        base_dir: Optional[Path] = None,
        eden_dir: Optional[Path] = None,
        etc_eden_dir: Optional[Path] = None,
        home_dir: Optional[Path] = None,
        logging_settings: Optional[Dict[str, str]] = None,
        extra_args: Optional[List[str]] = None,
        storage_engine: str = "memory",
        expected_fuse_transport: Optional[str] = None,
        command_prefix: Sequence[str] = (),
    ) -> None:
        """
        Construct a new EdenFS object.

        By default, all of the state directories needed for the edenfs daemon will be
        created under the directory specified by base_dir.  If base_dir is not
        specified, a temporary directory will be created.  The temporary directory will
        be removed when cleanup() or __exit__() is called on the EdenFS object.

        Explicit locations for various state directories (eden_dir, etc_eden_dir,
        home_dir) can also be given, if desired.  For instance, this allows an EdenFS
        object to be created for an existing eden state directory.
        """
        if base_dir is None:
            self._base_dir = Path(tempfile.mkdtemp(prefix="eden_test."))
            self._cleanup_base_dir = True
        else:
            self._base_dir = base_dir
            self._cleanup_base_dir = False

        if eden_dir is None:
            self._eden_dir = self._base_dir / "eden"
            self._eden_dir.mkdir(exist_ok=True)
        else:
            self._eden_dir = eden_dir

        if etc_eden_dir is None:
            self._etc_eden_dir = self._base_dir / "etc_eden"
            self._etc_eden_dir.mkdir(exist_ok=True)
        else:
            self._etc_eden_dir = etc_eden_dir

        if home_dir is None:
            self._home_dir = self._base_dir / "home"
            self._home_dir.mkdir(exist_ok=True)
        else:
            self._home_dir = home_dir

        self._storage_engine = storage_engine
        self._logging_settings = logging_settings
        self._extra_args = extra_args
        self.expected_fuse_transport = expected_fuse_transport
        self._command_prefix = command_prefix

        self._process: Optional[subprocess.Popen] = None

    @property
    def eden_dir(self) -> Path:
        return self._eden_dir

    @property
    def etc_eden_dir(self) -> Path:
        return self._etc_eden_dir

    @property
    def home_dir(self) -> Path:
        return self._home_dir

    @property
    def user_rc_path(self) -> Path:
        return self._home_dir / ".edenrc"

    @property
    def dynamic_rc_path(self) -> Path:
        return self._etc_eden_dir / "edenfs_dynamic.rc"

    @property
    def system_rc_path(self) -> Path:
        return self._etc_eden_dir / "edenfs.rc"

    @property
    def system_rollout_path(self) -> Path:
        return self._etc_eden_dir / "edenfsctl_rollout.json"

    def __enter__(self) -> "EdenFS":
        return self

    def __exit__(
        self, exc_type: type, exc_value: BaseException, tb: TracebackType
    ) -> bool:
        self.cleanup()
        return False

    def report_time(self, event: str) -> None:
        """
        report_time() is a helper function for logging how long different
        parts of the test took.

        Each time it is called it logs a message containing the time since the
        test started and the time since the last time report_time() was called.
        """
        now = time.time()
        since_last = now - self.last_event
        since_start = now - self.start_time
        logging.info("=== Eden %s at %.03fs (+%0.3fs)", event, since_start, since_last)
        self.last_event = now

    def cleanup(self) -> None:
        """Stop the instance and clean up its temporary directories"""
        self.kill()
        if self._cleanup_base_dir:
            shutil.rmtree(self._base_dir, ignore_errors=True)

    def kill(self, retry=False) -> None:
        """Stops and unmounts this instance."""
        process = self._process
        if process is None or process.returncode is not None:
            return
        self.shutdown(retry=retry)

    def wait_for_exit(self, timeout: float = EDENFS_STOP_TIMEOUT) -> int:
        process = self._process
        assert process is not None

        return_code = process.wait(timeout=timeout)
        self.report_time(f"Eden process exited with return code {return_code}")
        self._process = None
        return return_code

    def kill_dirty(self) -> None:
        """Kills privhelper and this instance directly, without waiting for cleanup.
        Used for testing recovery handlers."""
        process = self._process
        if process is None or process.returncode is not None:
            # EdenFS is not running
            return

        privhelperPid = self.get_privhelper_pid()
        if privhelperPid != -1:
            os.kill(privhelperPid, signal.SIGKILL)

        # Before shutting down, get the current pid. This may differ from process.pid when
        # edenfs is started with sudo.
        daemon_pid = util.check_health(
            self.get_thrift_client, self.eden_dir, timeout=30
        ).pid

        self._process = None

        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGKILL)
            # wait for process to exit
            timeout = 30
            while (
                util.check_health(
                    self.get_thrift_client, self.eden_dir, timeout=30
                ).status
                != fb303_status.DEAD
            ):
                time.sleep(5)
                timeout -= 5
                if timeout <= 0:
                    break
        else:
            process.kill()
        process.wait(timeout=10)

    @contextmanager
    def get_thrift_client(
        self, timeout: Optional[float] = None
    ) -> Generator[EdenService.Sync, None, None]:
        """Get a modern thrift-python sync client for communicating with EdenFS."""
        with client.create_thrift_client(
            eden_dir=str(self._eden_dir),
            timeout=timeout if timeout is not None else 0,
        ) as thrift_client:
            yield thrift_client

    def get_async_thrift_client(self, timeout: float = 0) -> EdenService.Async:
        return client.create_async_thrift_client(str(self._eden_dir), timeout=timeout)

    def run_cmd(
        self,
        command: str,
        *args: str,
        cwd: Optional[str] = None,
        capture_stderr: bool = False,
        encoding: str = "utf-8",
        config_dir: bool = True,
        env: Optional[Dict[str, str]] = None,
        timeout: Optional[float] = None,
    ) -> str:
        """
        Run the specified eden command.

        Args: The eden command name and any arguments to pass to it.
        Usage example: run_cmd('mount', 'my_eden_client')
        Throws a subprocess.CalledProcessError if eden exits unsuccessfully.
        With timeout, capture output in files so inherited pipes cannot prolong
        cleanup. Failures retain at most 64 KiB per stream; timeouts raise
        subprocess.TimeoutExpired with byte output, as subprocess.run does.
        """
        cmd, cmd_env = self.get_edenfsctl_cmd_env(command, *args, config_dir=config_dir)

        # Merge explicit env with env vars supplied by the test harness.
        # Explicitly provided env vars take precedence.
        merged_env = {}
        if env is not None:
            merged_env = cmd_env | env
        else:
            merged_env = cmd_env

        def read_output(stream: BinaryIO, limit: Optional[int] = None) -> bytes:
            # Snapshot the length: a descendant may still be writing.
            size = os.fstat(stream.fileno()).st_size
            stream.seek(0)
            return stream.read(size if limit is None else min(size, limit))

        def decode_output(data: bytes, errors: str = "strict") -> str:
            # Match subprocess.run's universal-newline text capture.
            return (
                data.decode(encoding, errors=errors)
                .replace("\r\n", "\n")
                .replace("\r", "\n")
            )

        # TODO(T37669726): Re-enable LSAN.
        merged_env["LSAN_OPTIONS"] = "detect_leaks=0:verbosity=1:log_threads=1"
        with ExitStack() as stack:
            stdout_file = (
                stack.enter_context(tempfile.TemporaryFile())
                if timeout is not None
                else None
            )
            stderr_file = (
                stack.enter_context(tempfile.TemporaryFile())
                if timeout is not None and not capture_stderr
                else None
            )
            try:
                completed_process = subprocess.run(
                    cmd,
                    stdout=stdout_file if stdout_file is not None else subprocess.PIPE,
                    stderr=(
                        subprocess.STDOUT
                        if capture_stderr
                        else stderr_file
                        if stderr_file is not None
                        else subprocess.PIPE
                    ),
                    check=True,
                    cwd=cwd,
                    env=merged_env,
                    encoding=encoding,
                    timeout=timeout,
                )
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as ex:
                if stdout_file is not None:
                    captured_stdout = read_output(stdout_file, 64 * 1024)
                    if isinstance(ex, subprocess.CalledProcessError):
                        ex.output = decode_output(captured_stdout, errors="replace")
                    else:
                        ex.output = captured_stdout
                if stderr_file is not None:
                    errors = read_output(stderr_file, 64 * 1024)
                    if isinstance(ex, subprocess.CalledProcessError):
                        ex.stderr = decode_output(errors, errors="replace")
                    else:
                        ex.stderr = errors
                if isinstance(ex, subprocess.CalledProcessError):
                    raise EdenCommandError(ex) from None
                raise
            output = (
                decode_output(read_output(stdout_file))
                if stdout_file is not None
                else completed_process.stdout
            )
        if config_dir and command in ("clone", "mount", "restart"):
            self.assert_running_fuse_transports()
        return output

    def run_unchecked(
        self, command: str, *args: str, **kwargs: Any
    ) -> subprocess.CompletedProcess:
        """Run the specified eden command.

        Args: The eden command name and any arguments to pass to it.
        Usage example: run_cmd('mount', 'my_eden_client')
        Returns a subprocess.CompletedProcess object.
        """
        cmd, edenfsctl_env = self.get_edenfsctl_cmd_env(command, *args)

        if "env" in kwargs:
            edenfsctl_env.update(kwargs["env"])
        kwargs["env"] = edenfsctl_env

        return subprocess.run(cmd, **kwargs)

    def get_edenfsctl_cmd_env(
        self,
        command: str,
        *args: str,
        config_dir: bool = True,
        home_dir: bool = True,
    ) -> Tuple[List[str], Dict[str, str]]:
        """Combines the specified eden command args with the appropriate
        defaults.

        Args:
            command: the eden command
            *args: extra string arguments to the command
        Returns:
            A list of arguments to run Eden that can be used with
            subprocess.Popen() or subprocess.check_call().
        """
        edenfsctl, env = FindExe.get_edenfsctl_env()
        cmd = [
            edenfsctl,
            "--etc-eden-dir",
            str(self._etc_eden_dir),
        ]
        if config_dir:
            cmd += ["--config-dir", str(self._eden_dir)]
        if home_dir:
            cmd += ["--home-dir", str(self._home_dir)]
        cmd.append(command)
        cmd.extend(args)
        return cmd, env

    def wait_for_is_healthy(self, timeout: float = EDENFS_START_TIMEOUT) -> bool:
        process = self._process
        assert process is not None
        health = util.wait_for_daemon_healthy(
            proc=process,
            config_dir=self._eden_dir,
            # pyre-ignore[6]: get_thrift_client is a context manager, compatible with check_health
            get_client=self.get_thrift_client,
            timeout=timeout,
        )
        self.assert_running_fuse_transports()
        return health.is_healthy()

    def start(
        self,
        timeout: float = EDENFS_START_TIMEOUT,
        takeover_from: Optional[int] = None,
        extra_args: Optional[List[str]] = None,
        preserved_env: Optional[List[str]] = None,
        extra_env: Optional[Dict[str, str]] = None,
        should_wait_for_daemon_healthy: bool = True,
    ) -> None:
        """
        Run "eden daemon" to start the eden daemon.

        Most everyone should use should_wait_for_daemon_healthy=True. The only
        reason not to use True is to avoid blocking on a restart. For example,
        You might want to avoid blocking on restart if you are injecting a
        blocking fault into restart/start.
        """
        self.start_time = time.time()
        self.last_event = self.start_time

        use_gdb = False
        if os.environ.get("EDEN_GDB"):
            use_gdb = True
            # Starting up under GDB takes longer than normal.
            # Allow an extra 90 seconds (for some reason GDB can take a very
            # long time to load symbol information, particularly on dynamically
            # linked builds).
            timeout += 90

        takeover = takeover_from is not None
        self.spawn_nowait(
            gdb=use_gdb,
            takeover=takeover,
            extra_args=extra_args,
            preserved_env=preserved_env,
            extra_env=extra_env,
        )

        process = self._process
        assert process is not None
        if should_wait_for_daemon_healthy:
            util.wait_for_daemon_healthy(
                proc=process,
                config_dir=self._eden_dir,
                # pyre-ignore[6]: get_thrift_client is a context manager, compatible with check_health
                get_client=self.get_thrift_client,
                timeout=timeout,
                exclude_pid=takeover_from,
            )
            # Takeover verification happens outside graceful_restart's rollback.
            if takeover_from is None:
                self.assert_running_fuse_transports()

    def get_extra_daemon_args(self) -> List[str]:
        extra_daemon_args: List[str] = []

        privhelper = FindExe.EDEN_PRIVHELPER
        if privhelper is not None:
            extra_daemon_args.extend(["--privhelper_path", privhelper])

        if "SANDCASTLE" in os.environ:
            extra_daemon_args.append("--allowRoot")

        # Turn up the VLOG level for the fuse server so that errors are logged
        # with an explanation when they bubble up to RequestData::catchErrors
        logging_settings = self._logging_settings
        if logging_settings:
            logging_arg = ",".join(
                "%s=%s" % (module, level)
                for module, level in sorted(logging_settings.items())
            )
            extra_daemon_args.extend(["--logging=" + logging_arg])
        extra_args = self._extra_args
        if extra_args:
            extra_daemon_args.extend(extra_args)

        # Tell the daemon where to find edenfsctl
        extra_daemon_args += ["--edenfsctlPath", FindExe.get_edenfsctl_env()[0]]

        return extra_daemon_args

    def spawn_nowait(  # noqa: C901
        self,
        gdb: bool = False,
        takeover: bool = False,
        preserved_env: Optional[List[str]] = None,
        extra_env: Optional[Dict[str, str]] = None,
        extra_args: Optional[List[str]] = None,
    ) -> None:
        """
        Start edenfs but do not wait for it to become healthy.
        """
        if self._process is not None:
            raise Exception("cannot start an already-running eden client")

        use_gdb = bool(os.environ.get("EDEN_GDB"))
        daemon_binary = FindExe.EDEN_DAEMON
        if use_gdb:
            # GDB needs the native executable, not Buck's environment wrapper.
            daemon_binary = os.path.abspath(
                os.environ.get("EDENFS_SERVER_BINARY", daemon_binary)
            )
        args, env = self.get_edenfsctl_cmd_env(
            "daemon", "--daemon-binary", daemon_binary, "--foreground"
        )

        if extra_env is not None:
            env.update(extra_env)

        extra_daemon_args = self.get_extra_daemon_args()
        if extra_args:
            extra_daemon_args.extend(extra_args)

        if takeover:
            args.append("--takeover")

        if preserved_env:
            args.append("--preserved-vars")
            args.extend(preserved_env)

        # If the EDEN_GDB environment variable is set, run eden inside gdb
        # so a developer can debug crashes
        if use_gdb:
            gdb_exit_handler = (
                "python gdb.events.exited.connect("
                "lambda event: "
                'gdb.post_event(lambda: gdb.execute("quit")) '
                'if getattr(event, "exit_code", None) == 0 '
                "else False"
                ")"
            )
            gdb_args = [
                # Register a handler to exit gdb if the program finishes
                # successfully. Queue the quit until exit notification finishes.
                "-ex",
                gdb_exit_handler,
            ]
            tsan_options = os.environ.get("EDENFS_DAEMON_TSAN_OPTIONS")
            if tsan_options:
                gdb_args.extend(["-ex", f"set environment TSAN_OPTIONS={tsan_options}"])
            # Start the program after applying the daemon-only environment.
            gdb_args.extend(["-ex", "run"])
            args.append("--gdb")
            for arg in gdb_args:
                args.append("--gdb-arg=" + arg)

        if "EDEN_DAEMON_ARGS" in os.environ:
            args.extend(shlex.split(os.environ["EDEN_DAEMON_ARGS"]))

        full_args = list(self._command_prefix) + args + ["--"] + extra_daemon_args
        logging.info(
            "Invoking eden daemon: %s", " ".join(shlex.quote(arg) for arg in full_args)
        )
        process = subprocess.Popen(
            full_args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            env=env,
            errors="surrogateescape",
        )

        # TODO(T69605343): Until TPX properly knows how to redirect writes done
        # to filedescriptors directly, we need to manually redirect EdenFS logs
        # to sys.std{out,err}.
        def redirect_stream(input_stream: TextIO, output_stream: TextIO) -> None:
            while True:
                line = input_stream.readline()
                if line == "":
                    input_stream.close()
                    return
                output_stream.write(line)

        threading.Thread(
            target=redirect_stream, args=(process.stdout, sys.stdout), daemon=True
        ).start()
        threading.Thread(
            target=redirect_stream, args=(process.stderr, sys.stderr), daemon=True
        ).start()

        self._process = process

    def _kill_after_shutdown_timeout(
        self, process: subprocess.Popen, daemon_pid: Optional[int]
    ) -> Tuple[int, bool]:
        if daemon_pid is not None and not is_process_running(daemon_pid):
            self.report_time(
                "Eden daemon exited; killing stale edenfsctl daemon wrapper"
            )
            process.kill()
            return_code = process.wait(timeout=10)
            self.report_time(
                f"Stale edenfsctl daemon wrapper exited with return code {return_code}"
            )
            return return_code, True

        if can_run_sudo() and daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGKILL)
        else:
            process.kill()
        return process.wait(timeout=10), False

    def shutdown(self, retry=False) -> None:
        """
        Run "eden shutdown" to stop the eden daemon.
        """
        process = self._process
        assert process is not None

        # Before shutting down, get the current pid. This may differ from process.pid when
        # edenfs is started with sudo.
        daemon_pid = util.check_health(
            self.get_thrift_client, self.eden_dir, timeout=30
        ).pid

        self.report_time(
            f"Shutting down eden with pid {process.pid}, daemon pid: {daemon_pid}"
        )

        timeout = EDENFS_STOP_TIMEOUT
        if retry:
            # If we know eden is already in a bad state use a more aggressive timeout value
            # to prevent overall test timeouts.
            timeout = 10

        deadline = time.monotonic() + timeout
        stop_completed = False
        stale_wrapper = False

        # Run "edenfsctl stop" with a timeout of 0 to tell it not to wait for the EdenFS
        # process to exit.  Since we are running it directly (self._process) we will
        # need to wait on it.  Depending on exactly how it is being run the process may
        # not go away until we wait on it.
        try:
            if process.returncode:
                # Process has already terminated.
                self.report_time(
                    f"Eden process has already terminated with return code {process.returncode}."
                )
                self._process = None
                return

            self.run_cmd(
                "stop", "-t", "0", timeout=max(0.0, deadline - time.monotonic())
            )
            stop_completed = True
            return_code = process.wait(timeout=max(0.0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired as ex:
            if not stop_completed:
                self.report_time(
                    f"edenfsctl stop timed out; stdout: {ex.output!r}; stderr: {ex.stderr!r}"
                )
            # EdenFS did not exit normally on its own.
            self.report_time("Eden stop timed out, killing process")
            return_code, stale_wrapper = self._kill_after_shutdown_timeout(
                process, daemon_pid
            )
            # An exited daemon does not make a stuck stop command successful.
            stale_wrapper = stale_wrapper and stop_completed
            if not retry and not stale_wrapper:
                raise Exception(
                    f"edenfs did not shutdown within {timeout} seconds; "
                    "had to send SIGKILL"
                )
        except EdenCommandError as e:
            # EdenFS did not exit normally on its own.
            self.report_time(f"Got EdenCommandError {e.returncode} {e.stderr}")
            if e.returncode == 2 and "edenfs is not running" in e.stderr:
                self.report_time("edenfs was not running, killing old process")
                if can_run_sudo() and daemon_pid is not None:
                    os.kill(daemon_pid, signal.SIGKILL)
                else:
                    process.kill()
                return_code = process.wait(timeout=10)
                self.report_time(f"process exited with return code {return_code}")
                if retry:
                    return
                else:
                    raise Exception(
                        f"edenfs did not shutdown within {timeout} seconds; "
                        "had to send SIGKILL"
                    )
            else:
                self.report_time(f"Hit unhandled EdenCommandError: {e}")
                raise
        finally:
            self._process = None

        # A stale wrapper is intentionally killed after the daemon has exited,
        # so its signal exit status does not indicate a daemon shutdown failure.
        if return_code != 0 and not retry and not stale_wrapper:
            raise Exception(
                "eden exited unsuccessfully with status {}".format(return_code)
            )
        self.report_time("Shutdown complete")

    def restart(self) -> None:
        self.shutdown()
        self.start()

    def get_pid_via_thrift(self) -> int:
        with self.get_thrift_client() as client:
            return client.getDaemonInfo().pid

    def get_privhelper_pid(self) -> int:
        with self.get_thrift_client(timeout=5) as client:
            privHelperInfo = client.checkPrivHelper()
            return privHelperInfo.pid

    def graceful_restart(
        self,
        timeout: float = EDENFS_START_TIMEOUT,
        should_wait_for_old: bool = True,
        should_wait_for_new: bool = True,
        extra_args: Optional[List[str]] = None,
    ) -> subprocess.Popen:
        """
        Roughly equivalent to `eden restart --graceful` to restart the daemon
        with takeover.

        Most everyone should use should_wait_for_daemon_healthy=True. The only
        reason not to use True is to avoid blocking on a restart. For example,
        You might want to avoid blocking on restart if you are injecting a
        blocking fault into restart.
        """

        old_process = self._process
        assert old_process is not None

        # Get the process ID of the old edenfs process.
        # Note that this is not necessarily self._process.pid, since the eden
        # CLI may have spawned eden using sudo, and self._process may refer to
        # a sudo parent process.
        old_pid = self.get_pid_via_thrift()

        self._process = None
        try:
            self.start(
                timeout=timeout,
                takeover_from=old_pid,
                should_wait_for_daemon_healthy=should_wait_for_new,
                extra_args=extra_args,
            )
        except Exception:
            # TODO: There might be classes of errors where the old_process is
            # gone and we do need to track the new process here.
            self._process = old_process
            raise

        # Check the return code from the old edenfs process
        if should_wait_for_old:
            return_code = old_process.wait()
            if return_code != 0:
                raise Exception(
                    "eden exited unsuccessfully with status {}".format(return_code)
                )
        if should_wait_for_new:
            self.assert_running_fuse_transports()
        return old_process

    def run_takeover_tool(self, cmd: List[str]) -> None:
        old_process = self._process
        assert old_process is not None

        subprocess.check_call(cmd)

        self._process = None
        return_code = old_process.wait()
        if return_code != 0:
            raise Exception(
                f"eden exited unsuccessfully with status {return_code} "
                "after a fake takeover stop"
            )

    def stop_with_stale_mounts(self) -> None:
        """Stop edenfs without unmounting any of its mount points.
        This will leave the mount points mounted but no longer connected to a FUSE
        daemon.  Attempts to access files or directories inside the mount will fail with
        an ENOTCONN error after this.
        """
        cmd: List[str] = [FindExe.TAKEOVER_TOOL, "--edenDir", str(self._eden_dir)]
        self.run_takeover_tool(cmd)

    def takeover_without_ping_response(self) -> None:
        """
        Execute a fake takeover to explicitly test a failed takeover. The
        takeover client does not send a ping with the nosendPing flag,
        so the subprocess call will throw, and we expect the old process
        to recover
        """
        cmd: List[str] = [
            FindExe.TAKEOVER_TOOL,
            "--edenDir",
            str(self._eden_dir),
            "--noshouldPing",
        ]

        try:
            subprocess.check_call(cmd)
        except Exception:
            # We expect the new process to fail starting.
            pass

    def list_cmd(
        self, env: Optional[Dict[str, str]] = None
    ) -> Dict[str, Dict[str, Any]]:
        """
        Executes "eden list --json" to list the Eden checkouts and returns the result as
        a dictionary.
        """
        data = self.run_cmd("list", "--json", env=env)
        return cast(Dict[str, Dict[str, Any]], json.loads(data))

    def list_cmd_simple(self, env: Optional[Dict[str, str]] = None) -> Dict[str, str]:
        """
        Executes "eden list --json" to list the Eden checkouts and returns the result in
        a simplified format that can be more easily used in test case assertions.

        The result is a dictionary of { mount_path: status }
        The status is a string containing one of the MountState names, or "NOT_RUNNING"
        if the mount is not running.  If the mount is known to the running edenfs
        instance but not listed in the configuration file, " (unconfigured)" will be
        appended to the status string.
        """
        results: Dict[str, str] = {}
        for path, mount_info in self.list_cmd(env).items():
            status_str = mount_info["state"]
            if not mount_info["configured"]:
                status_str += " (unconfigured)"
            results[path] = status_str

        return results

    def assert_running_fuse_transports(self) -> None:
        """Verify the configured transport expectation for every running FUSE mount.

        Mounts that are still initializing, failed, or unmounted are not checked.
        With no expectation configured, this does not contact the daemon.
        Successful devfuse fallback skips io_uring tests; other mismatches fail.
        A fallback on one mount does not hide a mismatch on another mount.
        """
        expected = self.expected_fuse_transport
        if expected is None:
            return
        with self.get_thrift_client(timeout=EDENFS_START_TIMEOUT) as thrift_client:
            assert_fuse_transports(thrift_client.listMounts(), expected)

    def get_mount_state(
        self, mount: pathlib.Path, client: Optional[EdenService.Sync] = None
    ) -> Optional[MountState]:
        """
        Query edenfs over thrift for the state of the specified mount.

        Returns the MountState enum, or None if edenfs does not currently know about
        this mount path.
        """
        if client is None:
            with self.get_thrift_client() as client:
                return self.get_mount_state(mount, client)
        else:
            for entry in client.listMounts():
                entry_path = pathlib.Path(os.fsdecode(entry.mountPoint))
                if entry_path == mount:
                    return MountState(entry.state)
            return None

    def wait_for_checkout_removed(
        self, mount: Union[str, os.PathLike], timeout: float = 60
    ) -> None:
        mount_path = pathlib.Path(mount)

        def checkout_removed() -> Optional[bool]:
            return True if self.get_mount_state(mount_path) is None else None

        util.poll_until(checkout_removed, timeout=timeout)

    async def get_mount_state_async(
        self, mount: pathlib.Path, client: Optional[EdenService.Async] = None
    ) -> Optional[MountState]:
        """
        Query edenfs over thrift for the state of the specified mount.

        Returns the MountState enum, or None if edenfs does not currently know about
        this mount path.
        """
        if client is None:
            async with self.get_async_thrift_client() as client:
                return await self.get_mount_state_async(mount, client)
        else:
            mounts = await client.listMounts()
            for entry in mounts:
                entry_path = pathlib.Path(os.fsdecode(entry.mountPoint))
                if entry_path == mount:
                    return entry.state
            return None

    def clone(
        self,
        repo: str,
        path: Union[str, os.PathLike],
        allow_empty: bool = False,
        case_sensitive: Optional[bool] = None,
        backing_store: Optional[str] = None,
        filter_paths: Optional[List[str]] = None,
    ) -> None:
        """
        Run "eden clone"
        """
        params = ["clone", repo, str(path)]
        if allow_empty:
            params.append("--allow-empty-repo")
        if case_sensitive:
            params.append("--case-sensitive")
        elif case_sensitive is False:  # Can also be None
            params.append("--case-insensitive")
        if backing_store is not None:
            params.append("--backing-store")
            params.append(backing_store)
        if (
            filter_paths is not None
            and len(filter_paths) != 0
            and any(p != "" for p in filter_paths)
        ):
            params.append("--filter-paths")
            params += filter_paths
        self.run_cmd(*params)

    def is_case_sensitive(self, path: Union[str, os.PathLike]) -> bool:
        """
        Return a checkout's case-sensitivity setting.
        """
        data = json.loads(self.run_cmd("info", str(path)))
        assert type(data["case_sensitive"]) is bool
        return data["case_sensitive"]

    def remove(self, path: str) -> None:
        """
        Run "eden remove <path>"
        """
        self.run_cmd("remove", "--yes", path)

    def in_proc_mounts(self, mount_path: str) -> bool:
        """Gets all eden mounts found in /proc/mounts, and returns
        true if this eden instance exists in list.
        """

        mount_path_bytes = mount_path.encode()
        if sys.platform == "linux":
            with open("/proc/mounts", "rb") as f:
                return any(
                    mount_path_bytes == line.split(b" ")[1]
                    for line in f.readlines()
                    if util.is_edenfs_mount_device(line.split(b" ")[0])
                )
        elif sys.platform == "darwin":
            allmounts = subprocess.check_output("mount")
            return any(
                mount_path_bytes == mount.mount_point
                for mount in mtab.parse_macos_mount_output(allmounts)
                if util.is_edenfs_mount(mount.device, mount.vfstype)
            )
        else:
            raise Exception(f"Unsupported platform: {sys.platform}")

    def is_healthy(self) -> bool:
        """Executes `eden health` and returns True if it exited with code 0."""
        cmd_result = self.run_unchecked("health")
        return cmd_result.returncode == 0

    def set_log_level(self, category: str, level: str) -> None:
        with self.get_thrift_client() as client:
            client.setOption("logging", f"{category}={level}")

    def client_dir_for_mount(self, mount_path: pathlib.Path) -> pathlib.Path:
        config_path = self.eden_dir / "config.json"
        with open(config_path, "r") as openedconfig:
            config = json.load(openedconfig)
        return pathlib.Path(self.eden_dir / "clients" / config[str(mount_path)])

    def overlay_dir_for_mount(self, mount_path: pathlib.Path) -> pathlib.Path:
        return self.client_dir_for_mount(mount_path) / "local"

    def mount(self, mount_path: pathlib.Path) -> None:
        self.run_cmd("mount", "--", str(mount_path))

    def unmount(self, mount_path: pathlib.Path) -> None:
        self.run_cmd("unmount", "--", str(mount_path))

    async def unmount_async(self, mount_path: pathlib.Path) -> None:
        async with self.get_async_thrift_client() as client:
            await client.unmount(os.fsencode(mount_path))


class EdenCommandError(subprocess.CalledProcessError):
    def __init__(self, ex: subprocess.CalledProcessError) -> None:
        super().__init__(ex.returncode, ex.cmd, output=ex.output, stderr=ex.stderr)

    def __str__(self) -> str:
        cmd_str = " ".join(shlex.quote(arg) for arg in self.cmd)
        return (
            "edenfsctl command returned non-zero exit status %d\n\nCommand:\n[%s]\n\nStderr:\n%s"
            % (
                self.returncode,
                cmd_str,
                self.stderr,
            )
        )


_can_run_eden: Optional[bool] = None
_can_run_fake_edenfs: Optional[bool] = None
_can_run_sudo: Optional[bool] = None


def can_run_eden(use_non_setuid_privhelper: bool = False) -> bool:
    """
    Determine if we can run eden.

    This is used to determine if we should even attempt running the
    integration tests.
    """
    global _can_run_eden
    can_run = _can_run_eden
    if can_run is None:
        can_run = _compute_can_run_eden(use_non_setuid_privhelper=False)
        _can_run_eden = can_run

    return can_run


def can_run_fake_edenfs(use_non_setuid_privhelper: bool = False) -> bool:
    """
    Determine if we can run the fake_edenfs helper program.

    This is similar to can_run_eden(), but does not require FUSE.
    """
    global _can_run_fake_edenfs
    can_run = _can_run_fake_edenfs
    if can_run is None:
        can_run = _compute_can_run_eden(
            require_fuse=False, use_non_setuid_privhelper=False
        )
        _can_run_fake_edenfs = can_run

    return can_run


def _compute_can_run_eden(
    require_fuse: bool = True, use_non_setuid_privhelper: bool = False
) -> bool:
    if "SANDCASTLE" in os.environ:
        # On Sandcastle, pretend that we can always run EdenFS, this prevents
        # blindspots where tests are suddenly skipped but still marked as
        # passed.
        return True

    if sys.platform == "win32":
        # On Windows ProjectedFS must be installed.
        # Our CMake configure step checks for the availability of ProjectedFSLib.lib
        # so that we can link against ProjectedFS at build time, but this doesn't
        # guarantee that ProjectedFS.dll is available.
        projfs_dll = r"C:\Windows\system32\ProjectedFSLib.dll"
        return os.path.exists(projfs_dll)

    # FUSE must be available
    if sys.platform == "linux" and require_fuse and not os.path.exists("/dev/fuse"):
        return False

    # If we're root, we won't have any privilege issues.
    if os.geteuid() == 0:
        return True

    # If we aren't using a setuid privhelper binary, we need sudo privileges.
    if use_non_setuid_privhelper:
        return can_run_sudo()

    # By default we use the system privhelper which is setuid-root, so we can
    # return True.
    return True


def can_run_sudo() -> bool:
    global _can_run_sudo
    can_run = _can_run_sudo
    if can_run is None:
        can_run = _compute_can_run_sudo()
        _can_run_sudo = can_run

    return can_run


def _compute_can_run_sudo() -> bool:
    if sys.platform == "win32":
        return False

    cmd = ["/usr/bin/sudo", "-E", "/usr/bin/true"]
    with open("/dev/null", "r") as dev_null:
        # Close stdout, stderr, and stdin, and call setsid() to make
        # sure we are detached from any controlling terminal.  This makes
        # sure that sudo can't prompt for a password if it needs one.
        # sudo will only succeed if it can run with no user input.
        process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=dev_null,
            preexec_fn=os.setsid,
        )
    process.communicate()
    return process.returncode == 0
