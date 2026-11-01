#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import contextlib
import datetime
import os
import re
import stat
import subprocess
import sys
import time
from pathlib import Path
from typing import Dict, Iterator, List, Optional, Tuple, Union

if sys.platform != "win32":
    import fcntl

from eden.fs.cli.util import (
    EdensparseMigrationStep,
    HEARTBEAT_FILE_PREFIX,
    maybe_edensparse_migration,
)

from . import configutil, daemon_util, error_report, proc_utils as proc_utils_mod
from .config import EdenInstance
from .util import get_pid_using_lockfile, poll_until, print_stderr, ShutdownError

# The amount of time to wait for the edenfs process to exit after we send SIGKILL.
# We normally expect the process to be killed and reaped fairly quickly in this
# situation.  However, in rare cases on very heavily loaded systems it can take a while
# for init/systemd to wait on the process and for everything to be fully cleaned up.
# Therefore we wait up to 30 seconds by default.  (I've seen it take up to a couple
# minutes on systems with extremely high disk I/O load.)
#
# If this timeout does expire this can cause `edenfsctl restart` to fail after
# killing the old process but without starting the new process, which is
# generally undesirable if we can avoid it.
DEFAULT_SIGKILL_TIMEOUT = 30.0

EDENFS_UNIT_NAME_TEMPLATE = "edenfs@{escaped_state_dir}.service"
EDENFS_SYSTEMD_SERVICE_UNIT = Path("/usr/lib/systemd/user/edenfs@.service")
EDENFS_SYSTEMD_SLICE_UNIT = Path("/usr/lib/systemd/user/edenfs.slice")
ENVIRONMENT_VARIABLE_NAME_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def _sanitize_unit_name(eden_dir: str) -> str:
    """Build a systemd unit name from the eden state directory path.

    Systemd treats '-' and ':' as special characters in unit names, so replace
    them with '_'.
    Append the current UNIX timestamp to avoid collisions during graceful restart
    where old and new scopes coexist briefly.
    """
    sanitized = re.sub(r"[^a-zA-Z0-9_./]", "_", eden_dir).strip("/").replace("/", "_")
    return f"edenfs_{sanitized}_{os.getpid()}_{int(time.time())}"


def _build_systemd_run_cmd(
    edenfs_cmd: List[str], eden_dir: str, eden_env: Dict[str, str]
) -> List[str]:
    """Wrap an edenfs command in systemd-run for cgroup isolation.

    Places edenfs in a transient scope under a dedicated eden.slice
    """
    unit_name = _sanitize_unit_name(eden_dir)
    cmd = [
        "systemd-run",
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--property=Delegate=yes",
        "--slice=edenfs",
        f"--unit={unit_name}",
    ]
    for name, value in eden_env.items():
        cmd.extend(["-E", f"{name}={value}"])
    cmd.append("--")
    cmd.extend(edenfs_cmd)
    return cmd


def _ensure_dbus_env(env: Dict[str, str]) -> bool:
    """Ensure D-Bus session vars are present and correct in *env*.

    Always computes the expected runtime directory from the current UID
    rather than trusting inherited environment variables, which may point
    to a different user's bus after `su`.

    Returns True if the vars are now set, False if the D-Bus socket does
    not exist.
    """
    uid = os.getuid()
    xdg_runtime_dir = f"/run/user/{uid}"
    dbus_socket = f"{xdg_runtime_dir}/bus"
    if not os.path.exists(dbus_socket):
        return False
    env["XDG_RUNTIME_DIR"] = xdg_runtime_dir
    env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={dbus_socket}"
    return True


def get_systemd_user_env() -> Optional[Dict[str, str]]:
    """Return os.environ with D-Bus vars ensured, or None if unavailable."""
    env = os.environ.copy()
    if not _ensure_dbus_env(env):
        return None
    return env


def _try_setup_systemd_env(
    systemd_env: Dict[str, str],
    instance: "EdenInstance",
) -> bool:
    """Prepare *systemd_env* for systemd --user commands via _ensure_dbus_env.

    Logs a telemetry sample and returns False (fall back to direct daemon
    management) when the D-Bus socket is unavailable; returns True otherwise.
    """
    if not _ensure_dbus_env(systemd_env):
        instance.log_sample(
            "systemd_setup",
            success=False,
            reason=f"dbus_socket_not_found at /run/user/{os.getuid()}/bus",
        )
        return False
    return True


def wait_for_process_exit(pid: int, timeout: float) -> bool:
    """Wait for the specified process ID to exit.

    Returns True if the process exits within the specified timeout, and False if the
    timeout expires while the process is still alive.
    """
    proc_utils: proc_utils_mod.ProcUtils = proc_utils_mod.new()

    def process_exited() -> Optional[bool]:
        if not proc_utils.is_process_alive(pid):
            return True
        return None

    try:
        poll_until(process_exited, timeout=timeout)
        return True
    except TimeoutError:
        return False


def wait_for_shutdown(
    pid: int,
    config_dir: Path,
    timeout: float,
    kill_timeout: float = DEFAULT_SIGKILL_TIMEOUT,
    instance: Optional["EdenInstance"] = None,
) -> bool:
    """Wait for a process to exit.

    If it does not exit within `timeout` seconds kill it with SIGKILL.
    Returns True if the process exited on its own or False if it only exited
    after SIGKILL.

    Throws a ShutdownError if we failed to kill the process with SIGKILL
    (either because we failed to send the signal, or if the process still did
    not exit within kill_timeout seconds after sending SIGKILL).
    """
    # Wait until the process exits on its own.
    if wait_for_process_exit(pid, timeout):
        return True

    # client.shutdown() failed to terminate the process within the specified
    # timeout.  Take a more aggressive approach by sending SIGKILL.
    print_stderr(
        "error: sent shutdown request, but edenfs did not exit "
        "within {} seconds. Attempting SIGKILL.",
        timeout,
    )
    sigkill_process(pid, config_dir, timeout=kill_timeout, instance=instance)
    return False


def _send_sigkill(
    pid: int,
    instance: Optional["EdenInstance"] = None,
) -> None:
    """Send SIGKILL to edenfs via systemctl or direct signal."""
    if instance is not None and sys.platform == "linux":
        try:
            unit = _get_systemd_unit(instance)
            if _is_systemd_unit_active(unit):
                systemd_env = get_systemd_user_env()
                subprocess.run(
                    ["systemctl", "--user", "kill", "--signal=KILL", unit],
                    env=systemd_env,
                )
                subprocess.run(
                    ["systemctl", "--user", "stop", unit],
                    env=systemd_env,
                )
                return
        except (RuntimeError, OSError) as ex:
            print_stderr(
                f"Failed to kill edenfs via systemctl, falling back to "
                f"direct signal: {ex}"
            )

    proc_utils: proc_utils_mod.ProcUtils = proc_utils_mod.new()
    try:
        proc_utils.kill_process(pid)
    except PermissionError as ex:
        raise ShutdownError(
            f"Received a permission error when attempting to kill edenfs: {ex}"
        )


def _may_remove_restart_sentinel(pid: int, config_dir: Path) -> bool:
    """Whether pid's killer may remove config_dir's restart sentinel.

    True when pid holds the lock, or when the lock file is absent so that no
    daemon owns the state dir. A lock file that exists but cannot be read or
    parsed counts as a live owner.
    """
    try:
        return get_pid_using_lockfile(config_dir) == pid
    except FileNotFoundError:
        return True
    except (OSError, ValueError):
        return False


@contextlib.contextmanager
def _restart_lock(config_dir: Path) -> Iterator[None]:
    """Serialize restart arming with a deliberate SIGKILL.

    Raises ShutdownError rather than proceeding unlocked: killing while a
    daemon arms would let the privhelper relaunch the daemon this kill is
    meant to retire.
    """
    lock_path = config_dir / daemon_util.RESTART_SENTINEL_LOCK_NAME
    with contextlib.ExitStack() as stack:
        try:
            lock_file = stack.enter_context(lock_path.open("a"))
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        except OSError as ex:
            raise ShutdownError(
                f"Failed to acquire restart lock {lock_path}: {ex}"
            ) from ex
        yield


def _remove_restart_sentinels(pid: int, config_dir: Path) -> None:
    prefix = f"{daemon_util.RESTART_SENTINEL_NAME_PREFIX}{pid}."
    try:
        sentinels = list(config_dir.glob(f"{prefix}*"))
    except OSError as ex:
        print_stderr(f"Failed to list restart sentinels in {config_dir}: {ex}")
        return
    for sentinel in sentinels:
        try:
            sentinel.unlink()
        except FileNotFoundError:
            pass
        except OSError as ex:
            print_stderr(f"Failed to delete restart sentinel {sentinel}: {ex}")


def sigkill_process(
    pid: int,
    config_dir: Path,
    timeout: float = DEFAULT_SIGKILL_TIMEOUT,
    instance: Optional["EdenInstance"] = None,
) -> None:
    """Send SIGKILL to a process, and wait for it to exit.

    If timeout is greater than 0, this waits for the process to exit after sending the
    signal.  Throws a ShutdownError exception if the process does not exit within the
    specified timeout.

    Returns successfully if the specified process did not exist in the first place.
    This is done to handle situations where the process exited on its own just before we
    could send SIGKILL.
    """

    # On Windows, EdenFS daemon doesn't have any heartbeat flag.
    if sys.platform != "win32":
        # This SIGKILL is not triggered by the OS due to memory issues, so we should clean up
        # the heartbeat file. This ensures that the SIGKILL won't be mislogged as a silent
        # exit when the next EdenFS daemon starts.
        # The thrift server may be unresponsive at this point, so clean up the file directly
        # instead of sending a thrift request.
        heartbeat_file = config_dir / f"{HEARTBEAT_FILE_PREFIX}{pid}"
        try:
            heartbeat_file.unlink()
        except FileNotFoundError:
            pass
        except Exception as e:
            print_stderr(f"Failed to delete heartbeat file {heartbeat_file}: {e}")

        if sys.platform == "darwin":
            with _restart_lock(config_dir):
                # Only pid's own sentinels are removed. A concurrent generation
                # that took ownership while this call waited is left alone.
                if _may_remove_restart_sentinel(pid, config_dir):
                    _remove_restart_sentinels(pid, config_dir)
                _send_sigkill(pid, instance)
        else:
            _send_sigkill(pid, instance)

    else:
        _send_sigkill(pid, instance)

    if timeout <= 0:
        return

    if not wait_for_process_exit(pid, timeout):
        raise ShutdownError(
            "edenfs process {} did not terminate within {} seconds of "
            "sending SIGKILL.".format(pid, timeout)
        )


def start_edenfs_service(
    instance: EdenInstance,
    daemon_binary: Optional[str] = None,
    edenfs_args: Optional[List[str]] = None,
    preserved_env: Optional[List[str]] = None,
) -> int:
    """Start the edenfs daemon."""
    return _start_edenfs_service(
        instance=instance,
        daemon_binary=daemon_binary,
        edenfs_args=edenfs_args,
        preserved_env=preserved_env,
        takeover=False,
    )


def gracefully_restart_edenfs_service(
    instance: EdenInstance,
    daemon_binary: Optional[str] = None,
    edenfs_args: Optional[List[str]] = None,
    preserved_env: Optional[List[str]] = None,
    force_takeover: bool = False,
    only_if_idle: bool = False,
) -> int:
    """
    Gracefully restart the EdenFS service
    This function ensures a seamless transition from the old EdenFS instance to the new one.

    The running daemon refuses a takeover while it is serving long Thrift
    requests. With force_takeover, it cancels them and proceeds. With
    only_if_idle, it also refuses while it has served a checkout or a mount
    recently.
    """
    # Unmount recovery and the health check's not-served scan run only while a
    # daemon is RUNNING, and neither daemon is RUNNING during the handover.
    return _start_edenfs_service(
        instance=instance,
        daemon_binary=daemon_binary,
        edenfs_args=edenfs_args,
        preserved_env=preserved_env,
        takeover=True,
        force_takeover=force_takeover,
        only_if_idle=only_if_idle,
    )


# Flags that apply to one takeover only. They travel with --undefok so that a
# daemon binary from an older package ignores them instead of refusing to
# start.
_UNDEFOK_TAKEOVER_FLAGS = "--undefok=forceTakeover,takeoverOnlyIfIdle"
_FORCE_TAKEOVER_FLAG = "--forceTakeover"
_ONLY_IF_IDLE_FLAG = "--takeoverOnlyIfIdle"
_TAKEOVER_ONLY_ARGS = [
    _UNDEFOK_TAKEOVER_FLAGS,
    _FORCE_TAKEOVER_FLAG,
    _ONLY_IF_IDLE_FLAG,
]

# Exit status of `eden restart --graceful` when the running daemon refused the
# takeover and kept serving, so that a caller that retries later, such as the
# restarter, can tell it from a restart that failed. This is EX_TEMPFAIL.
TAKEOVER_REFUSED_EXIT_CODE = 75

# The running daemon's refusal reaches the new daemon's startup log through
# the takeover error. Both refusal messages carry these words; the daemon
# keeps them in kTakeoverRefusedPhrase in EdenServer.cpp.
_TAKEOVER_REFUSED_MARKER = "refused the graceful restart"

# The error the daemon reported the last time a start failed, if any.
_last_daemon_error: Optional[str] = None


def _remember_daemon_error(daemon_error: Optional[str]) -> None:
    global _last_daemon_error
    _last_daemon_error = daemon_error


def takeover_was_refused() -> bool:
    """Whether the last daemon start failed because the running daemon refused
    the takeover."""
    return (
        _last_daemon_error is not None
        and _TAKEOVER_REFUSED_MARKER in _last_daemon_error
    )


def _takeover_only_flags(force_takeover: bool, only_if_idle: bool) -> List[str]:
    """The daemon flags that apply to one takeover."""
    if not (force_takeover or only_if_idle):
        return []
    flags = [_UNDEFOK_TAKEOVER_FLAGS]
    if force_takeover:
        flags.append(_FORCE_TAKEOVER_FLAG)
    if only_if_idle:
        flags.append(_ONLY_IF_IDLE_FLAG)
    return flags


def _drop_takeover_only_flags(
    instance: EdenInstance,
    cmd: List[str],
    eden_env: Dict[str, str],
    restart_cmd: List[str],
) -> None:
    """Rewrite the daemon args file without the flags meant for one takeover.

    systemd replays the file on every later reload, whether or not this one
    succeeded. A rewrite that fails leaves the file as it is: removing it would
    leave systemd nothing to start the daemon from.
    """
    plain_cmd = [arg for arg in cmd if arg not in _TAKEOVER_ONLY_ARGS]
    try:
        daemon_util.write_daemon_args_file(
            instance.state_dir, plain_cmd, eden_env, restart_cmd
        )
    except Exception as e:
        args_file = instance.state_dir / daemon_util.DAEMON_ARGS_FILENAME
        error_report.record(
            f"could not drop the takeover flags from the daemon args file: {e}"
        )
        print_stderr(
            "warning: could not drop the takeover flags from the daemon args "
            f"file {args_file}: {e}. A later systemctl reload of edenfs would "
            f"replay them. Remove {_FORCE_TAKEOVER_FLAG} and "
            f"{_ONLY_IF_IDLE_FLAG} from that file by hand, or run "
            "`eden restart --graceful` again once it is writable, which "
            "rewrites it."
        )


def _start_edenfs_with_systemd(
    instance: EdenInstance,
    systemd_env: Dict[str, str],
    takeover: bool,
    cmd: List[str],
    eden_env: Dict[str, str],
    restart_cmd: List[str],
    args_file_error: Optional[str],
) -> int:
    """Start or reload the daemon through systemd, which launches it from the
    daemon args file written just before."""
    if args_file_error is not None:
        # systemd launches the daemon from the file rather than from this
        # command, so there is nothing left for it to start.
        print_stderr(f"error: failed to write the daemon args file: {args_file_error}")
        error_report.record(f"failed to write the daemon args file: {args_file_error}")
        return 1
    try:
        return _systemctl_start_or_reload(instance, systemd_env, takeover)
    finally:
        if any(arg in _TAKEOVER_ONLY_ARGS for arg in cmd):
            _drop_takeover_only_flags(instance, cmd, eden_env, restart_cmd)


def _daemon_args_file(instance: EdenInstance) -> Path:
    return instance.state_dir / daemon_util.DAEMON_ARGS_FILENAME


def _try_write_daemon_args_file(
    instance: EdenInstance,
    cmd: List[str],
    eden_env: Dict[str, str],
    restart_cmd: Optional[List[str]] = None,
) -> Optional[str]:
    """Record the daemon command and environment in the state directory.

    Returns the failure message, or None if the file was written.
    """
    try:
        instance.state_dir.mkdir(parents=True, exist_ok=True)
        daemon_util.write_daemon_args_file(
            instance.state_dir, cmd, eden_env, restart_cmd
        )
    except OSError as e:
        with contextlib.suppress(OSError):
            _daemon_args_file(instance).unlink(missing_ok=True)
        return str(e)
    except Exception as e:
        # best-effort
        return str(e)
    return None


def _warn_daemon_args_file_failure(instance: EdenInstance, error: str) -> None:
    """Warn that the daemon command could not be recorded."""
    # A state directory that refuses the write usually refuses the removal of
    # the earlier file too, which leaves that command armed instead of none.
    # An unreadable state directory gets the same benefit of the doubt.
    try:
        stale_file_survives = _daemon_args_file(instance).exists()
    except OSError:
        stale_file_survives = True
    consequence = (
        "edenfs may be auto-restarted from an earlier start's command"
        if stale_file_survives
        else "edenfs will not be auto-restarted after a crash"
    )
    print_stderr(
        f"warning: failed to write the daemon args file: {error}; {consequence}"
    )


def _start_edenfs_service(
    instance: EdenInstance,
    daemon_binary: Optional[str] = None,
    edenfs_args: Optional[List[str]] = None,
    preserved_env: Optional[List[str]] = None,
    takeover: bool = False,
    force_takeover: bool = False,
    only_if_idle: bool = False,
) -> int:
    """Get the command and environment to use to start edenfs."""
    _remember_daemon_error(None)
    daemon_binary = daemon_util.find_daemon_binary(daemon_binary)
    cmd, privhelper = get_edenfs_cmd(instance, daemon_binary)

    # Snapshot the relaunch command before `--takeover` and the sudo wrapper,
    # neither of which may be replayed. Must stay above
    # `prepare_edenfs_privileges`.
    restart_cmd = list(cmd)

    if takeover:
        cmd.append("--takeover")
        # The args file written below is replayed by systemd, possibly by a
        # daemon binary from an older package after a downgrade, which the
        # --undefok in these flags accounts for.
        cmd.extend(_takeover_only_flags(force_takeover, only_if_idle))
    if edenfs_args:
        # A restarted daemon should behave like the one it replaces.
        cmd.extend(edenfs_args)
        restart_cmd.extend(edenfs_args)

    eden_env = get_edenfs_environment(instance, preserved_env)
    systemd_env = os.environ.copy()

    # Wrap the command in sudo, if necessary. See help text in
    # prepare_edenfs_privileges for more info.
    cmd, eden_env = prepare_edenfs_privileges(daemon_binary, cmd, eden_env, privhelper)

    use_systemd = should_use_systemd_lifecycle_management(
        instance
    ) and _try_setup_systemd_env(systemd_env, instance)

    # Windows has neither systemd nor a privhelper, so nothing there reads the
    # file back. Off systemd the only reader is privhelper restart, which the
    # daemon arms from this same knob.
    write_args_file = sys.platform != "win32" and (
        use_systemd
        or instance.get_config_bool("privhelper.restart-edenfs-on-crash", default=False)
    )

    args_file_error = (
        _try_write_daemon_args_file(instance, cmd, eden_env, restart_cmd)
        if write_args_file
        else None
    )

    if use_systemd:
        return _start_edenfs_with_systemd(
            instance,
            systemd_env,
            takeover,
            cmd,
            eden_env,
            restart_cmd,
            args_file_error,
        )

    if args_file_error is not None:
        _warn_daemon_args_file_failure(instance, args_file_error)

    if (
        sys.platform == "linux"
        and instance.get_config_bool(
            "experimental.systemd-cgroup-isolation", default=False
        )
        and _try_setup_systemd_env(systemd_env, instance)
    ):
        cmd = _build_systemd_run_cmd(cmd, str(instance.state_dir), eden_env)
        launch_env = systemd_env
        use_systemd_cgroup = True
    else:
        launch_env = eden_env
        use_systemd_cgroup = False

    maybe_edensparse_migration(instance, EdensparseMigrationStep.PRE_EDEN_START)
    exit_code, daemon_error = _launch_daemon(instance, cmd, launch_env)
    _remember_daemon_error(daemon_error)
    maybe_edensparse_migration(instance, EdensparseMigrationStep.POST_EDEN_START)
    if exit_code != 0:
        error_report.record(
            daemon_error or f"edenfs daemon startup exited with status {exit_code}"
        )

    if use_systemd_cgroup:
        instance.log_sample(
            "systemd_cgroup_start",
            success=exit_code == 0,
            is_takeover=takeover,
            exit_signal=exit_code,
        )

    return exit_code


def _get_systemd_unit(instance: EdenInstance) -> str:
    """Compute the systemd unit name for this instance.

    Uses systemd-escape to produce the canonical path encoding for the state
    directory.  Raises if systemd-escape is not available or fails.
    """
    config_dir = str(instance.state_dir)
    try:
        escaped = subprocess.check_output(
            ["systemd-escape", "--path", config_dir],
            text=True,
        ).strip()
    except FileNotFoundError:
        raise RuntimeError(
            "systemd-escape is not installed; cannot construct systemd unit name"
        )
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"systemd-escape failed for {config_dir}: {e}") from e
    return EDENFS_UNIT_NAME_TEMPLATE.format(escaped_state_dir=escaped)


def should_use_systemd_lifecycle_management(instance: EdenInstance) -> bool:
    """Check whether this EdenFS instance should use systemd for lifecycle management.

    Returns True only when all of the following are satisfied:
    1. Running on Linux
    2. The config key experimental.systemd-managed-lifecycle is true
    3. The systemd unit files are installed on disk
    """
    if sys.platform != "linux":
        return False
    if not instance.get_config_bool(
        "experimental.systemd-managed-lifecycle", default=False
    ):
        return False
    return _systemd_unit_files_installed()


def _systemd_unit_files_installed() -> bool:
    """Check whether the required systemd unit files are installed on disk."""
    missing = [
        str(p)
        for p in (EDENFS_SYSTEMD_SERVICE_UNIT, EDENFS_SYSTEMD_SLICE_UNIT)
        if not p.exists()
    ]
    if missing:
        print_stderr(
            f"warning: systemd-managed-lifecycle is enabled in config but "
            f"the following unit files were not found: {', '.join(missing)}. "
            f"Falling back to direct daemon management."
        )
        return False
    return True


def _is_systemd_unit_active(unit: str) -> bool:
    """Check whether the systemd unit for this instance is currently active."""
    try:
        result = subprocess.run(
            ["systemctl", "--user", "is-active", "--quiet", unit],
            env=get_systemd_user_env(),
        )
        return result.returncode == 0
    except OSError:
        return False


def print_systemd_status_full(instance: "EdenInstance") -> None:
    """Print full systemctl status output for this instance's systemd unit."""
    try:
        unit = _get_systemd_unit(instance)
        subprocess.run(
            ["systemctl", "--user", "status", "--no-pager", unit],
            env=get_systemd_user_env(),
        )
    except (RuntimeError, OSError) as e:
        print_stderr(f"warning: failed to get status of edenfs service unit: {e}")
        return


def _rotate_startup_log(log_path: Path, keep: int = 5) -> None:
    """Rotate the startup log, keeping the last `keep` copies.

    Renames the current log to include its last-modified timestamp, then
    removes the oldest copies beyond `keep`.
    """
    try:
        if not log_path.exists():
            return
        mtime = log_path.stat().st_mtime
        ts = datetime.datetime.fromtimestamp(mtime).strftime("%Y%m%d_%H%M%S")
        rotated = log_path.with_name(f"{log_path.name}.{ts}")
        log_path.rename(rotated)
    except OSError as e:
        print_stderr(f"warning: failed to rotate startup log: {e}")
        # If we can't rotate, just truncate.
        try:
            log_path.write_text("")
        except OSError as e2:
            print_stderr(f"warning: failed to truncate startup log: {e2}")
        return

    # Prune old rotated logs beyond the retention limit.
    try:
        prefix = f"{log_path.name}."
        rotated_logs = sorted(
            (p for p in log_path.parent.iterdir() if p.name.startswith(prefix)),
            key=lambda p: p.name,
            reverse=True,
        )
        for old in rotated_logs[keep:]:
            old.unlink(missing_ok=True)
    except OSError as e:
        print_stderr(f"warning: failed to prune old startup logs: {e}")


def _extract_daemon_error(startup_log_content: str) -> Optional[str]:
    """Return the error portion of the daemon's startup log.

    Lines before the "Starting edenfs" marker are XLOG noise emitted before
    redirectOutput() and are not useful for diagnostics.  Returns None if the
    marker is absent or there is no content after it.
    """
    lines = startup_log_content.split("\n")
    for i, line in enumerate(lines):
        if line.startswith("Starting edenfs"):
            filtered = "\n".join(lines[i + 1 :]).strip()
            return filtered if filtered else None
    return None


def _new_startup_log(instance: EdenInstance) -> Path:
    """Rotate the earlier startup logs. Return the log path for this start."""
    startup_log = instance.state_dir / daemon_util.STARTUP_LOG_FILENAME
    # TODO: make the retention count configurable
    _rotate_startup_log(startup_log, keep=5)
    return startup_log


def _read_startup_log(startup_log: Path, start_time: float) -> Optional[str]:
    """Return the startup log content if the log changed at or after `start_time`.

    Return None if the log is older, missing, or unreadable. An older log is
    the output of an earlier start. It stays if the rotation failed.
    """
    try:
        if startup_log.exists() and startup_log.stat().st_mtime >= start_time:
            return startup_log.read_text()
    except OSError as e:
        print_stderr(f"warning: failed to read startup log: {e}")
    return None


_STARTUP_NOTICE = (
    "Starting EdenFS. The startup output will show when the start is complete."
)


def _launch_daemon(
    instance: EdenInstance, cmd: List[str], env: Dict[str, str]
) -> Tuple[int, Optional[str]]:
    """Run the daemon launch command.

    Outside Windows, the command writes its stderr to the startup log. The log
    shows on stderr when the command exits. Return the exit code. On failure,
    also return the daemon error from the log.
    """
    if sys.platform == "win32":
        return subprocess.call(cmd, stdin=subprocess.DEVNULL, env=env), None

    startup_log = _new_startup_log(instance)
    start_time = time.time()
    try:
        instance.state_dir.mkdir(parents=True, exist_ok=True)
        # Use a file, not a pipe. The privhelper inherits this stderr and can
        # keep a pipe open after the daemon command exits.
        log_file = open(startup_log, "wb")
    except OSError as e:
        print_stderr(f"warning: failed to open the startup log: {e}")
        return subprocess.call(cmd, stdin=subprocess.DEVNULL, env=env), None
    print_stderr(_STARTUP_NOTICE)
    with log_file:
        exit_code = subprocess.call(
            cmd, stdin=subprocess.DEVNULL, stderr=log_file, env=env
        )

    content = _read_startup_log(startup_log, start_time)
    if content:
        sys.stderr.write(content)
    if exit_code == 0 or not content:
        return exit_code, None
    return exit_code, _extract_daemon_error(content)


def _systemctl_start_or_reload(
    instance: EdenInstance,
    systemd_env: Dict[str, str],
    takeover: bool,
) -> int:
    """Start or reload the edenfs systemd service."""
    unit = _get_systemd_unit(instance)
    if takeover and _is_systemd_unit_active(unit):
        action = "reload"
    else:
        action = "start"

    # systemd appends to the startup log (StandardOutput=file:). Without the
    # rotation, output of earlier starts goes into the CLI output.
    startup_log = _new_startup_log(instance)

    print_stderr(_STARTUP_NOTICE)
    start_time = time.time()
    result = subprocess.run(
        ["systemctl", "--user", action, unit],
        capture_output=True,
        text=True,
        env=systemd_env,
    )
    rc = result.returncode

    # Display the daemon's startup output captured by systemd (StandardOutput=file:).
    startup_log_content = _read_startup_log(startup_log, start_time)
    if startup_log_content:
        sys.stderr.write(startup_log_content)

    sample_kwargs: Dict[str, Union[bool, int, str]] = {
        "action": action,
        "success": rc == 0,
        "exit_code": rc,
    }
    if result.stderr:
        sample_kwargs["error"] = result.stderr
    if rc != 0:
        if result.stderr:
            print_stderr(result.stderr)
        # Always check the startup log for daemon errors independently of
        # systemctl stderr — systemctl's generic "Job failed" message doesn't
        # contain the actual daemon error (e.g. "error starting EdenFS: ...").
        daemon_error = None
        if startup_log_content:
            daemon_error = _extract_daemon_error(startup_log_content)
            if daemon_error:
                sample_kwargs["daemon_startup_log"] = daemon_error
        _remember_daemon_error(daemon_error)
        error_report.record(
            daemon_error or result.stderr or f"systemctl {action} {unit} failed"
        )
    instance.log_sample("systemctl_action", **sample_kwargs)
    return rc


def get_edenfsctl_cmd() -> str:
    env = os.environ.get("EDENFS_CLI_PATH", None)
    if env:
        return env

    env = os.environ.get("__EDENFSCTL_RUST", None)
    if env:
        return env

    edenfsctl_real = os.path.abspath(sys.argv[0])
    if sys.platform == "win32":
        edenfsctl = os.path.join(edenfsctl_real, "../edenfsctl.exe")
    else:
        edenfsctl = os.path.join(edenfsctl_real, "../edenfsctl")
    return os.path.normpath(edenfsctl)


def get_edenfs_cmd(
    instance: EdenInstance,
    daemon_binary: str,
) -> Tuple[List[str], str]:
    """Get the command line arguments to use to start the edenfs daemon."""

    cmd = [
        daemon_binary,
        "--edenfs",
        "--edenfsctlPath",
        get_edenfsctl_cmd(),
        "--edenDir",
        str(instance.state_dir),
        "--etcEdenDir",
        str(instance.etc_eden_dir),
        "--configPath",
        str(instance.user_config_path),
    ]

    privhelper_path = os.environ.get("EDENFS_PRIVHELPER_PATH")
    # TODO(cuev): Avoid hardcoding the privhelper path. Instead, we should
    # share candidate paths with FindExe (which is used in integration tests)
    if privhelper_path is None:
        # Default to using the system privhelper. See explanation below.
        privhelper_path = "/usr/local/libexec/eden/edenfs_privhelper"

    cmd += ["--privhelper_path", privhelper_path]

    return cmd, privhelper_path


def prepare_edenfs_privileges(
    daemon_binary: str,
    cmd: List[str],
    env: Dict[str, str],
    privhelper_path: str,
) -> Tuple[List[str], Dict[str, str]]:
    """Update the EdenFS command and environment settings in order to run the
    privhelper as root. Note: in most cases, we don't need to do anything to
    run the privhelper as root since we ship it as a setuid-root binary. This
    is the default case/behavior.

    However, sometimes we need to test non-setuid-root privhelper binaries in
    dev instances of EdenFS or integration tests. In those cases, we need to
    wrap the command in sudo.

    This happens when a non-setuid-root binary is specified by the
    EDENFS_PRIVHELPER_PATH environment variable. In most cases, this env
    variable will not be set and we will simply use the system privhelper. This
    environment variable is set by some development Buck targets in
    fbcode/eden/fs/{cli, cli_rs}/TARGETS. It could also potentially be set by
    other external sources.
    """
    # Nothing to do on Windows
    if sys.platform == "win32":
        return (cmd, env)

    # If we already have root privileges we don't need to do anything.
    if os.geteuid() == 0:
        return (cmd, env)

    # If the EdenFS privhelper is installed as setuid root we don't need to use
    # sudo.
    try:
        s = os.stat(privhelper_path)
        if s.st_uid == 0 and (s.st_mode & stat.S_ISUID):
            return (cmd, env)
    except FileNotFoundError:
        # If the privhelper isn't found, EdenFS would just fail, let it fail
        # instead of here.
        return cmd, env

    # If we're still here, we need to run edenfs under sudo. This is
    # undesirable with passwordless sudo as it requires multiple password
    # prompts, but we will try to run with sudo anyway.
    # In some rare cases, we may want to test using a non-setuid-root
    # privhelper binary.
    sudo_cmd = ["/usr/bin/sudo"]
    # Add environment variable settings
    # Depending on the sudo configuration, these may not
    # necessarily get passed through automatically even when
    # using "sudo -E".
    for key, value in env.items():
        sudo_cmd.append("%s=%s" % (key, value))

    cmd = sudo_cmd + cmd
    return cmd, env


def _apply_configured_environment(
    instance: EdenInstance, eden_env: Dict[str, str]
) -> None:
    configured_environment = instance.get_config_strs(
        "daemon.environment", default=configutil.Strs([])
    )
    _apply_environment_entries(configured_environment, eden_env)


def _apply_environment_entries(
    configured_environment: configutil.Strs, eden_env: Dict[str, str]
) -> None:
    for index, entry in enumerate(configured_environment):
        name, separator, value = entry.partition("=")
        if not separator or not name:
            print_stderr(
                "warning: ignoring invalid daemon.environment entry at index "
                f"{index}: expected NAME=value with a non-empty name"
            )
            continue
        if "\0" in name or "\0" in value:
            print_stderr(
                f"warning: ignoring invalid daemon.environment entry at index {index}: "
                "names and values must not contain NUL"
            )
            continue
        if ENVIRONMENT_VARIABLE_NAME_RE.fullmatch(name) is None:
            print_stderr(
                f"warning: ignoring invalid daemon.environment entry at index {index}: "
                "name must contain only ASCII letters, digits, and underscores, and "
                "must not start with a digit"
            )
            continue
        eden_env[name] = value


def get_edenfs_environment(
    instance: EdenInstance,
    extra_preserve: Optional[List[str]],
) -> Dict[str, str]:
    """Get the environment to use to start the edenfs daemon."""
    eden_env = {}

    # Errors from Rust will be logged to the edenfs log.
    eden_env["SL_LOG"] = (
        "clienttelemetry=info,error,walkdetector=info,backingstore::prefetch=info,indexedlog::rotate=info"
    )

    if sys.platform != "win32":
        # Reset $PATH to the following contents, so that everyone has the
        # same consistent settings.
        path_dirs = ["/opt/facebook/hg/bin", "/usr/local/bin", "/bin", "/usr/bin"]

        eden_env["PATH"] = ":".join(path_dirs)
    else:
        # On Windows, copy the existing PATH as it's not clear what locations
        # are needed.
        eden_env["PATH"] = os.environ["PATH"]

    if sys.platform == "darwin":
        # Prevent warning on mac, which will crash eden:
        # +[__NSPlaceholderDate initialize] may have been in progress in
        # another thread when fork() was called.
        eden_env["OBJC_DISABLE_INITIALIZE_FORK_SAFETY"] = "YES"

    # Setup EdenFs service id
    eden_env["FB_SERVICE_ID"] = "scm/edenfs"

    # Preserve the following environment settings
    #
    # NOTE: This list should be expanded sparingly. Prefer ad-hoc passing of
    # preserved environment variables via the Eden CLI. For example:
    #   edenfsctl --preserved-vars EXAMPE_VAR EXAMPLE_VAR2 start
    # This will ensure that environment variables are only preserved when
    # explicitly requested.
    preserve = [
        "USER",
        "LOGNAME",
        "HOME",
        "EMAIL",
        "NAME",
        "ASAN_OPTIONS",
        # When we import data from mercurial, the remotefilelog extension
        # may need to SSH to a remote mercurial server to get the file
        # contents.  Preserve SSH environment variables needed to do this.
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "KRB5CCNAME",
        "ATLAS",
        # Identifier of dev docker containers
        "ATLAS_ENV_ID",
        "CI_PURPOSE",
        "SANDCASTLE",
        "SANDCASTLE_ALIAS",
        "SANDCASTLE_INSTANCE_ID",
        "SANDCASTLE_VCS",
        "SCRATCH_CONFIG_PATH",
        # These environment variables are used by Corp2Prod (C2P) Secure Thrift
        # clients to get the user certificates for authentication. (We use
        # C2P Secure Thrift to fetch metadata from SCS).
        "THRIFT_TLS_CL_CERT_PATH",
        "THRIFT_TLS_CL_KEY_PATH",
        # This helps with rust debugging
        "MISSING_FILES",
        "EDENSCM_LOG",
        "EDENSCM_EDENAPI",
        "RUST_BACKTRACE",
        "RUST_LIB_BACKTRACE",
        "SL_LOG",  # alias for EDENSCM_LOG
        # Useful for environments that look like prod, but are actually corp
        "CONFIGERATOR_PRETEND_NOT_PROD",
        # Ensure EdenFS respects redirecting which cache directory to write to
        "XDG_CACHE_HOME",
        # EdenFS should be able to pick-up Mercurial's test config
        "HG_TEST_REMOTE_CONFIG",
        # In some environment, this is used instead of the USER variable
        "CLOUD2PROD_IDENTITY",
        # The following 4 are used on sl's .t tests
        "HGRCPATH",
        "SL_CONFIG_PATH",
        "TESTTMP",
        "HGUSER",
        # The following are used to identify RE platform
        "REMOTE_EXECUTION_SCM_REPO",
        "INSIDE_RE_WORKER",
        # Used by tests to trigger error conditions in instrumented Rust code.
        "FAILPOINTS",
        # systemd-run --user needs these to connect to the user session bus
        # when starting edenfs with cgroup isolation.
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ]

    if sys.platform == "win32":
        preserve += [
            "APPDATA",
            "SYSTEMROOT",
            "USERPROFILE",
            "USERNAME",
            "PROGRAMDATA",
            "LOCALAPPDATA",
        ]

    for name, value in os.environ.items():
        # Preserve any environment variable starting with "TESTPILOT_".
        # TestPilot uses a few environment variables to keep track of
        # processes started during test runs, so it can track down and kill
        # runaway processes that weren't cleaned up by the test itself.
        # We want to make sure this behavior works during the eden
        # integration tests.
        # Similarly, we want to preserve EDEN* env vars which are
        # populated by our own test infra to relay paths to important
        # build artifacts in our build tree.
        if name.startswith("TESTPILOT_") or name.startswith("EDEN"):
            eden_env[name] = value
        elif name in preserve:
            eden_env[name] = value
        else:
            # Drop any environment variable not matching the above cases
            pass

    _apply_configured_environment(instance, eden_env)
    if extra_preserve is not None:
        for name in extra_preserve:
            if name in os.environ:
                eden_env[name] = os.environ[name]
    return eden_env
