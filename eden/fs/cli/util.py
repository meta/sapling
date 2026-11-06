#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import abc
import asyncio
import binascii
import enum
import errno
import functools
import getpass
import json
import os
import random
import re
import shlex
import stat
import struct
import subprocess
import sys
import time
import typing
from io import BytesIO
from pathlib import Path
from typing import (
    Any,
    Awaitable,
    Callable,
    Coroutine,
    Dict,
    Iterator,
    List,
    Optional,
    TextIO,
    TYPE_CHECKING,
    TypeVar,
)

from eden.fs.service.eden.thrift_types import TreeInodeDebugInfo
from eden.thrift.client import EdenNotRunningError
from fb303_core.thrift_types import fb303_status
from thrift.python.exceptions import Error, TransportError

if TYPE_CHECKING:
    from .config import EdenCheckout, EdenInstance

if sys.platform != "win32":
    import pwd


class EdensparseMigrationStep(enum.Enum):
    PRE_EDEN_START = "pre_eden_start"
    POST_EDEN_START = "post_eden_start"


MIGRATION_MARKER = "edensparse_migration"


class RepoError(Exception):
    pass


# These paths are relative to the user's client directory.
LOCK_FILE = "lock"
PID_FILE = "pid"
HEARTBEAT_FILE_PREFIX = "heartbeat_"

NFS_MOUNT_PROTOCOL_STRING = "nfs"
FUSE_MOUNT_PROTOCOL_STRING = "fuse"
PRJFS_MOUNT_PROTOCOL_STRING = "prjfs"

CHEF_LOG_PATH_DARWIN = "/var/chef/outputs/chef.last.run_stats"
CHEF_LOG_PATH_LINUX = "/var/chef/outputs/chef.last.run_stats"
CHEF_LOG_PATH_WIN32 = "C:\\chef\\outputs\\chef.last.run_stats"


# These are files in a client directory
CLONE_SUCCEEDED = "clone-succeeded"
MOUNT_CONFIG = "config.toml"
SNAPSHOT = "SNAPSHOT"
INTENTIONALLY_UNMOUNTED = "intentionally-unmounted"
SNAPSHOT_MAGIC_1 = b"eden\x00\x00\x00\x01"
SNAPSHOT_MAGIC_2 = b"eden\x00\x00\x00\x02"
SNAPSHOT_MAGIC_3 = b"eden\x00\x00\x00\x03"
SNAPSHOT_MAGIC_4 = b"eden\x00\x00\x00\x04"
NULL_FILTER = b"null"


class EdenStartError(Exception):
    pass


class ShutdownError(Exception):
    pass


class NotAnEdenMountError(Exception):
    def __init__(self, path: str) -> None:
        self.path = path

    def __str__(self) -> str:
        return f"{self.path} does not appear to be inside an EdenFS checkout"


class HealthStatus:
    def __init__(
        self,
        status: fb303_status,
        pid: Optional[int],
        uptime: Optional[float],
        detail: str,
    ) -> None:
        self.status = status
        self.pid = pid  # The process ID, or None if not running
        self.uptime = uptime
        self.detail = detail  # a human-readable message

    def is_healthy(self) -> bool:
        return self.status == fb303_status.ALIVE

    def is_starting(self) -> bool:
        return self.status == fb303_status.STARTING

    def __str__(self) -> str:
        return "(%s, pid=%s, uptime=%s, detail=%r)" % (
            self.status.name
            if isinstance(self.status, fb303_status)
            else str(self.status),
            self.pid,
            self.uptime,
            self.detail,
        )


T = TypeVar("T")


def run_async_func_in_thread(
    async_func: Callable[..., Coroutine[typing.Any, typing.Any, T]],
    *args: Any,
) -> None:
    asyncio.run(async_func(*args))


async def poll_until_async(
    function: Callable[[], Awaitable[Optional[T]]],
    timeout: float,
    interval: float = 0.2,
    timeout_ex: Optional[Exception] = None,
) -> T:
    """
    Call the specified awaitable function repeatedly until it returns non-None.
    Returns the function result.

    Sleep 'interval' seconds between calls.  If 'timeout' seconds passes
    before the function returns a non-None result, raise an exception.
    If a 'timeout_ex' argument is supplied, that exception object is
    raised, otherwise a TimeoutError is raised.
    """
    end_time = time.time() + timeout
    while True:
        result = await function()

        if result is not None:
            return result

        if time.time() >= end_time:
            if timeout_ex is not None:
                raise timeout_ex
            raise TimeoutError(
                "timed out waiting on function {}".format(function.__name__)
            )

        await asyncio.sleep(interval)


def poll_until(
    function: Callable[[], Optional[T]],
    timeout: float,
    interval: float = 0.2,
    timeout_ex: Optional[Exception] = None,
) -> T:
    """
    Call the specified function repeatedly until it returns non-None.
    Returns the function result.

    Sleep 'interval' seconds between calls.  If 'timeout' seconds passes
    before the function returns a non-None result, raise an exception.
    If a 'timeout_ex' argument is supplied, that exception object is
    raised, otherwise a TimeoutError is raised.
    """
    end_time = time.time() + timeout
    while True:
        result = function()
        if result is not None:
            return result

        if time.time() >= end_time:
            if timeout_ex is not None:
                raise timeout_ex
            raise TimeoutError(
                "timed out waiting on function {}".format(function.__name__)
            )

        time.sleep(interval)


def get_pid_using_lockfile(config_dir: Path) -> int:
    """Read the pid from the EdenFS lockfile, throwing an exception upon failure."""
    if sys.platform == "win32":
        # On Windows read the separate pid file.  We will not be able to read the
        # lock file if EdenFS is running and holding the lock.
        lockfile = config_dir / PID_FILE
    else:
        # On other platforms we still prefer reading the pid from the lock file,
        # just to support older instances of EdenFS that only wrote the pid to the lock
        # file.
        lockfile = config_dir / LOCK_FILE

    with lockfile.open("r") as f:
        lockfile_contents = f.read()
    return int(lockfile_contents.rstrip())


def check_health_using_lockfile(config_dir: Path) -> HealthStatus:
    """Make a best-effort to produce a HealthStatus based on the PID in the
    EdenFS lockfile.
    """
    try:
        # Throws if it does not parse as an int.
        pid = get_pid_using_lockfile(config_dir)
    except Exception:
        # If we cannot read the PID from the lockfile for any reason, return
        # DEAD.
        return _create_dead_health_status()

    from . import proc_utils as proc_utils_mod

    proc_utils = proc_utils_mod.new()
    if proc_utils.is_edenfs_process(pid):
        return HealthStatus(
            fb303_status.STOPPED,
            pid,
            uptime=None,
            detail="EdenFS's Thrift server does not appear to be "
            "running, but the process is still alive (PID=%s)." % pid,
        )
    else:
        return _create_dead_health_status()


def _create_dead_health_status() -> HealthStatus:
    return HealthStatus(
        fb303_status.DEAD, pid=None, uptime=None, detail="EdenFS not running"
    )


def check_health(
    get_client: Callable[..., Any],
    config_dir: Path,
    timeout: Optional[float] = None,
) -> HealthStatus:
    """
    Get the status of the edenfs daemon.

    Returns a HealthStatus object containing health information.
    """
    if timeout is None:
        # Default to a 3 second timeout if an explicit value was not specified
        timeout = 3.0

    pid = None
    status = fb303_status.DEAD
    uptime = None
    try:
        with get_client(timeout=timeout) as client:
            info = client.getDaemonInfo()
            pid = info.pid
            status_value = info.status
            # Our wrapper client class always ensures that info.status is present.
            # It will explicitly call getStatus() to get this information if the
            # daemon is running an older version that does not return this from
            # getDaemonInfo()
            assert status_value is not None
            status = status_value
            uptime = info.uptime
    except (
        EdenNotRunningError,
        TransportError,
    ):
        # It is possible that the edenfs process is running, but the Thrift
        # server is not running. This could be during the startup, shutdown,
        # or takeover of the edenfs process. As a backup to requesting the
        # PID from the Thrift server, we read it from the lockfile and try
        # to deduce the current status of EdenFS.
        return check_health_using_lockfile(config_dir)
    except Error as ex:
        detail = "error talking to edenfs: " + str(ex)
        return HealthStatus(status, pid, uptime, detail)

    status_name = status.name
    detail = "edenfs running (pid {}); status is {}".format(pid, status_name)
    return HealthStatus(status, pid, uptime, detail)


def wait_for_daemon_healthy(
    proc: subprocess.Popen[bytes],
    config_dir: Path,
    get_client: Callable[..., Any],
    timeout: float,
    exclude_pid: Optional[int] = None,
) -> HealthStatus:
    """
    Wait for edenfs to become healthy.
    """

    def check_daemon_health() -> Optional[HealthStatus]:
        # Check the thrift status
        health_info = check_health(get_client, config_dir)
        if health_info.is_healthy():
            if (exclude_pid is None) or (health_info.pid != exclude_pid):
                return health_info

        # Make sure that edenfs is still running
        status = proc.poll()
        if status is not None:
            if status < 0:
                msg = "terminated with signal {}".format(-status)
            else:
                msg = "exit status {}".format(status)
            raise EdenStartError("edenfs exited before becoming healthy: " + msg)

        # Still starting
        return None

    timeout_ex = EdenStartError("timed out waiting for edenfs to become healthy")
    return poll_until(check_daemon_health, timeout=timeout, timeout_ex=timeout_ex)


def wait_for_instance_healthy(instance: "EdenInstance", timeout: float) -> HealthStatus:
    """
    Wait for EdenFS to become healthy. This method differs from wait_for_daemon_healthy
    because wait_for_daemon_healthy is used for EdenFS instances spawned by a direct
    child, while this method is for use to wait on an existent process.
    """
    from . import proc_utils as proc_utils_mod

    proc_utils = proc_utils_mod.new()

    # pyre-fixme[53]: Captured variable `proc_utils` is not annotated.
    def check_daemon_health() -> Optional[HealthStatus]:
        # Check the thrift status
        health_info = instance.check_health()
        if health_info.is_healthy():
            return health_info

        # Make sure that the edenfs process is still alive
        pid = health_info.pid
        if pid is None or not proc_utils.is_process_alive(pid):
            raise EdenStartError("edenfs exited before becoming healthy")

        # Still starting
        return None

    timeout_ex = EdenStartError("timed out waiting for edenfs to become healthy")
    return poll_until(check_daemon_health, timeout=timeout, timeout_ex=timeout_ex)


def get_chef_log_path(platform: str) -> Optional[str]:
    """Get the path to the Chef log file."""
    if platform == "Darwin":
        return CHEF_LOG_PATH_DARWIN
    elif platform == "Linux":
        return CHEF_LOG_PATH_LINUX
    elif platform == "Windows":
        return CHEF_LOG_PATH_WIN32
    return None


def get_home_dir() -> Path:
    # NOTE: Path.home() should work on all platforms, but we would want
    # careful about making that change in case users have muddled with
    # their HOME env var or if the resolution is weird in a containerized
    # environment. It would be worth having some external logging to count
    # mismatches between the two approaches
    home_dir = None
    if sys.platform == "win32":
        home_dir = os.getenv("USERPROFILE")
        if not home_dir:
            return Path.home()
    else:
        home_dir = os.getenv("HOME")
        if not home_dir:
            try:
                home_dir = pwd.getpwuid(os.getuid()).pw_dir
            except Exception:
                return Path.home()
    return Path(home_dir)


def mkdir_p(path: str) -> str:
    """Performs `mkdir -p <path>` and returns the path."""
    try:
        os.makedirs(path)
    except OSError as e:
        if e.errno != errno.EEXIST:
            raise
    return path


class Repo(abc.ABC):
    HEAD: str = "Must be defined by subclasses"

    def __init__(
        self, type: str, source: str, working_dir: Optional[str] = None
    ) -> None:
        # The repository type: 'hg' or 'git'
        self.type = type
        # The repository data source.
        # For mercurial this is the directory containing .hg/store
        # For git this is the repository .git directory
        self.source = source
        # The root of the working directory
        self.working_dir = working_dir

    def __repr__(self) -> str:
        return (
            f"Repo(type={self.type!r}, source={self.source!r}, "
            f"working_dir={self.working_dir!r})"
        )

    @abc.abstractmethod
    def get_commit_hash(self, commit: str) -> str:
        """
        Returns the commit hash for the given hg revision ID or git
        commit-ish.
        """
        pass


class HgRepo(Repo):
    HEAD = "."

    def __init__(
        self,
        source: str,
        working_dir: Optional[str] = None,
        backing_type: Optional[str] = None,
    ) -> None:
        super(HgRepo, self).__init__(
            backing_type if backing_type is not None else "hg",
            source,
            source if working_dir is None else working_dir,
        )
        # pyre-fixme[4]: Attribute must be annotated.
        self._env = os.environ.copy()
        self._env["HGPLAIN"] = "1"

        # These are set by the par machinery and interfere with Mercurial's
        # own dynamic library loading.
        self._env.pop("DYLD_INSERT_LIBRARIES", None)
        self._env.pop("DYLD_LIBRARY_PATH", None)

        # Find the path to hg.
        # The EDEN_HG_BINARY environment variable is normally set when running
        # Eden's integration tests.  Just find 'hg' from the path when it is
        # not set.
        # pyre-fixme[4]: Attribute must be annotated.
        self._hg_binary = os.environ.get("EDEN_HG_BINARY", "hg")

    def __repr__(self) -> str:
        return f"HgRepo(source={self.source!r}, working_dir={self.working_dir!r})"

    # pyre-fixme[2]: Parameter must be annotated.
    def _run_hg(
        self,
        args: List[str],
        stderr_output: Optional[int] = None,
        cwd: Optional[str] = None,
    ) -> bytes:
        if cwd is None:
            cwd = self.working_dir
        cmd = [self._hg_binary] + args
        out_bytes = subprocess.check_output(
            cmd, cwd=cwd, env=self._env, stderr=stderr_output
        )
        out = out_bytes
        return out

    # pyre-fixme[2]: Parameter must be annotated.
    def get_commit_hash(self, commit: str, stderr_output=None) -> str:
        out = self._run_hg(["log", "-r", commit, "-T{node}"], stderr_output)
        return out.strip().decode("utf-8")


class HttpRepo(Repo):
    HEAD = "HEAD"

    def __init__(self, source: str, working_dir: Optional[str] = None) -> None:
        super(HttpRepo, self).__init__("http", source, working_dir)

    def __repr__(self) -> str:
        return f"HttpRepo(source={self.source!r})"

    def get_commit_hash(self, commit: str) -> str:
        raise NotImplementedError(
            "get_commit_hash is not supposed to be called for HttpRepo"
        )


def mkscratch_bin() -> Path:
    # mkscratch is provided by the hg deployment at facebook, which has a
    # different installation prefix on macOS vs Linux, so we need to resolve
    # it via the PATH.  In the integration test environment we'll set the
    # MKSCRATCH_BIN to point to the binary under test
    return Path(os.environ.get("MKSCRATCH_BIN", "mkscratch"))


def get_hg_repo(path: str, backing_type: Optional[str] = None) -> Optional[HgRepo]:
    """
    If path points to a mercurial repository, return a HgRepo object.
    Otherwise, if path is not a mercurial repository, return None.
    """
    repo_path = path
    working_dir = path
    from . import hg_util

    hg_dir = os.path.join(repo_path, hg_util.sniff_dot_dir(Path(path)))
    if not os.path.isdir(hg_dir):
        return None

    # Check to see if this is a shared working directory from another
    # repository
    try:
        with open(os.path.join(hg_dir, "sharedpath"), "r") as f:
            hg_dir = f.readline().rstrip("\n")
            hg_dir = os.path.realpath(hg_dir)
            repo_path = os.path.dirname(hg_dir)
    except EnvironmentError as ex:
        if ex.errno != errno.ENOENT:
            raise

    if not os.path.isdir(os.path.join(hg_dir, "store")):
        return None

    return HgRepo(repo_path, working_dir, backing_type)


def get_http_repo(path: str) -> Optional[HttpRepo]:
    """
    Return a HttpRepo object, with the source path.
    """
    return HttpRepo(path)


def get_repo(path: str, backing_store_type: Optional[str] = None) -> Optional[Repo]:
    """
    Given a path inside a repository, return the repository source and type.
    """
    if backing_store_type == "http":
        # The repository for http is the server backend (host:port).
        # Skip checking if the local path exists with the repo source name.
        return get_http_repo(path)

    path = os.path.realpath(path)
    if not os.path.exists(path):
        return None

    while True:
        hg_repo = get_hg_repo(path, backing_store_type)
        if hg_repo is not None:
            return hg_repo

        parent = os.path.dirname(path)
        if parent == path:
            return None

        path = parent


def print_stderr(message: str, *args: Any, **kwargs: Any) -> None:
    """Prints the message to stderr."""
    if args or kwargs:
        message = message.format(*args, **kwargs)
    print(message, file=sys.stderr)


def stack_trace() -> str:
    import traceback

    return "".join(traceback.format_stack())


def is_valid_sha1(sha1: str) -> bool:
    """True iff sha1 is a valid 40-character SHA1 hex string."""
    if sha1 is None or len(sha1) != 40:
        return False
    import string

    return set(sha1).issubset(string.hexdigits)


def get_eden_mount_name(path_arg: str) -> str:
    """
    Get the path to the EdenFS checkout containing the specified path
    """
    if sys.platform == "win32":
        try:
            path = path_arg
            parent = os.path.dirname(path)
            while path != parent:
                # A checkout's `.eden` holds a `config` file; the EdenFS state
                # directory `~/.eden` is also named `.eden` and holds none.
                if os.path.isfile(os.path.join(path, ".eden", "config")):
                    return os.path.realpath(path)
                from . import hg_util

                if os.path.exists(
                    os.path.join(path, hg_util.sniff_dot_dir(Path(path)))
                ):
                    break
                path = parent
                parent = os.path.dirname(path)
            raise NotAnEdenMountError(path_arg)
        except OSError as e:
            # WinError 369 is "The provider that supports file system
            # virtualization is temporarily unavailable". This usually
            # indicates the path is leftover of a previous EdenFS mount.
            if e.winerror == 369:
                raise NotAnEdenMountError(path_arg)
            raise
    else:
        path = os.path.join(path_arg, ".eden", "root")
        try:
            return os.readlink(path)
        except OSError as ex:
            if ex.errno == errno.ENOTDIR:
                path = os.path.join(os.path.dirname(path_arg), ".eden", "root")
                return os.readlink(path)
            elif ex.errno == errno.ENOENT:
                raise NotAnEdenMountError(path_arg)
            raise


def get_username() -> str:
    return getpass.getuser()


class LoadedNode(typing.NamedTuple):
    path: str
    is_write: bool
    file_size: int


def make_loaded_node(path: str, is_write: bool, file_size: Optional[int]) -> LoadedNode:
    assert file_size is not None, "File should have associated file size"
    return LoadedNode(path=path, is_write=is_write, file_size=file_size)


def split_inodes_by_operation_type(
    inode_results: typing.Sequence[TreeInodeDebugInfo],
) -> typing.Tuple[
    typing.List[typing.Tuple[str, int]], typing.List[typing.Tuple[str, int]]
]:
    loaded_node_info = [
        make_loaded_node(
            path=os.path.join(os.fsdecode(tree.path), os.fsdecode(n.name)),
            is_write=n.materialized or not n.hash,
            file_size=n.fileSize,
        )
        for tree in inode_results
        for n in tree.entries
        if n.loaded and stat.S_IFMT(n.mode) == stat.S_IFREG
    ]

    read_files = [(o.path, o.file_size) for o in loaded_node_info if not o.is_write]
    written_files = [(o.path, o.file_size) for o in loaded_node_info if o.is_write]
    return read_files, written_files


def fdatasync(fd: int) -> None:
    # pyre-fixme [16]: Undefined attribute
    getattr(os, "fdatasync", os.fsync)(fd)


def write_file_atomically(path: Path, contents: bytes) -> None:
    "Atomically writes or replaces a file at path with the given contents."
    tmp = path.with_suffix(".tmp" + hex(random.getrandbits(64))[2:])
    try:
        with tmp.open("xb") as f:
            f.write(contents)
            f.flush()
            fdatasync(f.fileno())
        os.replace(tmp, path)
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def format_cmd(cmd: bytes) -> str:
    args = os.fsdecode(cmd)

    # remove trailing null which would cause the command to show up with an
    # extra empty string on the end
    args = re.sub("\x00$", "", args)

    args = args.split("\x00")

    # Focus on just the basename as the paths can be quite long
    cmd_str: str = args[0]
    if os.path.isabs(cmd):
        cmd_str = os.path.basename(cmd_str)

    # Show cmdline args too, if they exist
    return " ".join(shlex.quote(p) for p in [cmd_str] + args[1:])


def format_mount(mount: bytes) -> str:
    return os.fsdecode(os.path.basename(mount))


def underlined(message: str) -> str:
    line = "-" * len(message)
    return f"\n{message}\n{line}\n"


def is_edenfs_mount_device(device: bytes) -> bool:
    return device == b"eden" or device == b"edenfs" or device.startswith(b"edenfs:")


def is_edenfs_unix_socket_nfs_device(device: bytes) -> bool:
    """Whether the device of an NFS mount names EdenFS's nfsd socket.

    macOS reports an NFS mount served over a Unix domain socket as
    "<socket path>:<mount path>". Keep in sync with
    is_edenfs_unix_socket_nfs_source() in eden/common/utils/FSDetect.h.
    """
    return device.startswith(b"<") and b"/edenfs-nfsd.socket>:" in device


def is_edenfs_mount(device: bytes, vfstype: bytes) -> bool:
    """Whether a mount table entry is an EdenFS checkout.

    On macOS the kernel replaces the device of an NFS mount served over a
    Unix domain socket with the socket path, so the "edenfs:" device alone
    does not identify EdenFS there. The socket name does, and so does the
    filesystem type when EdenFS overrides it to "edenfs:".
    """
    return (
        is_edenfs_mount_device(device)
        or vfstype == b"edenfs:"
        or (vfstype == b"nfs" and is_edenfs_unix_socket_nfs_device(device))
    )


def get_eden_cli_cmd(argv: List[str] = sys.argv) -> List[str]:
    # We likely only need to do this on windows to make sure we run the
    # edenfsctl in a python environment that isn't frozen. But this should
    # be safe to do on all platforms, so for sake of uniformity, do it
    # everywhere.
    return [sys.executable] + argv[:1]


# some processes like hg and arc are sensitive about their environments, we
# clear variables that might cause problems for their dynamic linking.
# note buck is even more sensitive see buck.run_buck_command
def get_environment_suitable_for_subprocess() -> Dict[str, str]:
    env = os.environ.copy()

    # Clean out par related environment so that we don't cause problems
    # for our child process
    for k in os.environ.keys():
        if k in ("DYLD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES", "PAR_LAUNCH_TIMESTAMP"):
            del env[k]
        elif k.startswith("FB_PAR") or k.startswith("PYTHON"):
            del env[k]

    return env


def x2p_enabled() -> bool:
    result = subprocess.run(
        ["hg", "config", "auth_proxy.x2pagentd"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )

    # "hg config" exits 1 if config isn't set. If there is some other error,
    # let's be conservative and run the x509 check.
    if result.returncode:
        return False

    return result.stdout.strip().lower() in {b"1", b"t", b"true"}


def is_sandcastle() -> bool:
    return "SANDCASTLE" in os.environ


def is_remote_execution() -> bool:
    return os.environ.get("REMOTE_EXECUTION_SCM_REPO") == "1"


def is_atlas() -> bool:
    return "ATLAS" in os.environ


def get_platform_default_mount_protocol() -> str:
    if sys.platform == "win32":
        return PRJFS_MOUNT_PROTOCOL_STRING
    elif sys.platform == "darwin":
        return NFS_MOUNT_PROTOCOL_STRING
    else:
        return FUSE_MOUNT_PROTOCOL_STRING


def get_protocol(nfs: bool) -> str:
    if sys.platform == "win32":
        return PRJFS_MOUNT_PROTOCOL_STRING
    else:
        return NFS_MOUNT_PROTOCOL_STRING if nfs else FUSE_MOUNT_PROTOCOL_STRING


def get_tip_commit_hash(repo: Path) -> bytes:
    # Try to get the tip commit ID.  If that fails, use the null commit ID.
    args = ["hg", "log", "-T", "{node}", "-r", "tip"]
    env = dict(os.environ, HGPLAIN="1")
    result = subprocess.run(
        args,
        env=env,
        cwd=str(repo),
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return binascii.unhexlify(result.stdout.strip())


class Spinner:
    """
    A small utility class for displaying a progress spinner during long running
    operations with optional additional text.
    """

    def __init__(self, header: str) -> None:
        self._header = header
        self._cursor: Iterator[str] = self.cursor()
        self._file: Optional[TextIO] = sys.stderr
        if self._file and not self._file.isatty():
            self._file = None

    @staticmethod
    def cursor() -> Iterator[str]:
        while True:
            for cursor in "|/-\\":
                yield cursor

    def spin(self, text: str = "") -> None:
        if f := self._file:
            f.write("\r\033[K")
            f.write(f"{self._header} {text} ")
            f.write(f"{next(self._cursor)} ")
            f.flush()

    def __enter__(self) -> "Spinner":
        return self

    # pyre-fixme[2]: Parameter must be annotated.
    def __exit__(self, ex_type, ex_value, ex_traceback) -> bool:
        if f := self._file:
            f.write("\n")
            f.flush()
        return False


# pyre-fixme[3]: Return type must be annotated.
def hook_recursive_with_spinner(
    function: Callable,  # pyre-fixme[24]: Generic type `Callable` expects 2 type parameters.
    spinner: Spinner,
    args_parser: Callable,  # pyre-fixme[24]: Generic type `Callable` expects 2 type parameters.
):
    """
    hook_recursive_with_spinner
    Hook a recursive function updating a spinner at every recursion step
    Params:
    - function: the recursive function to hook
    - spinner: Spinner supporting text arguments
    - args_parser: a callable to extract printable information from args
    """

    @functools.wraps(function)
    # pyre-fixme[3]: Return type must be annotated.
    # pyre-fixme[2]: Parameter must be annotated.
    def run(*args, **kwargs):
        spinner.spin(args_parser(args))
        return function(*args, **kwargs)

    return run


if sys.platform == "win32":

    def remove_unc_prefix(path: Path) -> Path:
        parts = list(path.parts)
        if re.match(r"\\\\\?\\[A-Za-z]:\\", parts[0]):
            parts[0] = parts[0][-3:].upper()
        return Path(*parts)


# pyre-fixme[3]: Return type must be annotated.
def _varint_byte(b: int):
    return bytes((b,))


def encode_varint(number: int) -> bytes:
    """Pack `number` into varint bytes"""
    buf = b""
    while True:
        towrite = number & 0x7F
        number >>= 7
        if number:
            buf += _varint_byte(towrite | 0x80)
        else:
            buf += _varint_byte(towrite)
            break
    return buf


# Adapted from https://github.com/fmoo/python-varint and
# https://fburl.com/p8xrmnch
def decode_varint(buf: bytes) -> typing.Tuple[int, int]:
    """Read a varint from from `buf` bytes and return the number of bytes read"""
    stream = BytesIO(buf)
    shift = 0
    result = 0
    bytes_read = 0

    # pyre-fixme[3]: Return type must be annotated.
    def read_one_byte(stream: BytesIO):
        """Reads a byte from the file (as an integer)

        raises EOFError if the stream ends while reading bytes.
        """
        c = stream.read(1)
        if c == b"":
            raise EOFError("Unexpected EOF while reading varint bytes")
        return ord(c)

    while True:
        bytes_read += 1
        i = read_one_byte(stream)
        result |= (i & 0x7F) << shift
        shift += 7
        if not (i & 0x80):
            break

    return result, bytes_read


def create_legacy_filter_id(root_id: str, filter_path: Optional[str]) -> bytes:
    return (
        f"{filter_path}:{root_id}".encode("utf-8")
        if filter_path is not None
        else b"null"
    )


def create_filtered_rootid(root_id: str, filter_id: bytes) -> bytes:
    """Create a FilteredRootId from a RootId and filter path pair.

    The FilteredRootId is in the form:

    <Varint><OriginalRootId><FilterId>

    Where the Varint represents the length of the OriginalRootId and the
    FilterId represents which filter should be applied to the checkout. If no
    filter is provided, the "null" filter is used.
    """
    original_len = len(root_id)
    varint = encode_varint(original_len)
    return varint + root_id.encode() + filter_id


def maybe_edensparse_migration(
    instance: "EdenInstance", step: EdensparseMigrationStep
) -> None:
    """
    edensparse migration include two steps:
    1. Manipulate eden state files including:
       - Update content from SNAPSHOT file to apply the 'null' filter
       - Update config toml file to switch scm_type to "filteredhg"
    2. Update Sapling config files including:
       - Create an empty .hg/sparse file
       - Add "edensparse" to .hg/requires
       - Config Sapling to disable "extensions.sparse" and enable "extensions.edensparse"
       - Finally create an empty marker file to indicate part 1 of migration is complete
    """
    if step not in {
        EdensparseMigrationStep.PRE_EDEN_START,
        EdensparseMigrationStep.POST_EDEN_START,
    }:
        raise RuntimeError(f"Invalid edensparse migration step {step}")

    # Backup file names for cross-step rollback during edensparse migration
    SNAPSHOT_BACKUP: str = "SNAPSHOT.edensparse_backup"
    CONFIG_TOML_BACKUP: str = "config.toml.edensparse_backup"

    def log(msg: str) -> None:
        """
        Logs a migration message if verbose logging is enabled via the
        EDENSPARSE_MIGRATION_VERBOSE_LOGGING environment variable.
        """
        if os.environ.get("EDENSPARSE_MIGRATION_VERBOSE_LOGGING"):
            print(f"edensparse_migration: {msg}")
        else:
            pass

    def should_migrate(
        checkout: "EdenCheckout", step: EdensparseMigrationStep = step
    ) -> bool:
        """
        We only run edensparse migration for checkout if a special sapling config
        is set for the backing repo.
        """
        SL_CONFIG_TO_ALLOW_MIGRATION = "experimental.allow-edensparse-migration"
        sl_args = ["config", "-Tjson", SL_CONFIG_TO_ALLOW_MIGRATION]
        try:
            output = json.loads(checkout.get_backing_repo()._run_hg(sl_args))
        except Exception as ex:
            log(
                f"failed to determine whether to migrate {checkout.name}: {ex}; "
                "skipping migration"
            )
            return False

        if len(output) == 0:
            log(
                f"{SL_CONFIG_TO_ALLOW_MIGRATION} not set for {checkout.name}, skipping migration"
            )
            return False  # config not set for this repo, skip migration

        if output[0]["value"] != "true":  # config is set but disabled
            log(
                f"{SL_CONFIG_TO_ALLOW_MIGRATION}: {output[0]['value']} != 'true', skipping migration"
            )
            return False

        log(f"starting {step} for checkout: {checkout.name}")
        return True

    num_successful_migration = 0
    migration_exceptions = []

    for checkout in instance.get_checkouts():
        if not should_migrate(checkout):
            # verbose logging already in `should_migrate`
            continue

        rollbacks = []  # rollback callables

        fault_injector = NaiveFaultInjector(checkout.state_dir)

        def configure_sapling(
            checkout: "EdenCheckout",
            rollbacks: List[Any] = rollbacks,
            fault_injector: NaiveFaultInjector = fault_injector,
        ) -> bool:
            """
            Configure Sapling to disable "extensions.sparse" and enable "extensions.edensparse"

            Return True if the config is updated
            Return False if no config changes made
            """

            def hg(args: List[str], checkout: "EdenCheckout" = checkout) -> bytes:
                return checkout.get_backing_repo()._run_hg(args, cwd=str(checkout.path))

            def check_sl_config_value(key: str, value: Optional[str]) -> bool:
                """
                Check if the config value is set to the given value

                Return True if the config value is set to the given value
                Return False if the config value is not set to the given value
                """
                output = json.loads(hg(["config", "-Tjson", key]))
                if len(output) == 0:
                    # config value is not set
                    return value is None
                return output[0]["value"] == value

            is_sparse_disabled: bool = check_sl_config_value("extensions.sparse", "!")
            is_edensparse_enabled: bool = check_sl_config_value(
                "extensions.edensparse", ""
            )

            def restore_hgrc() -> None:
                hg(
                    [
                        "config",
                        "--local",
                        "extensions.sparse",
                        "!" if is_sparse_disabled else "",
                    ]
                )

                hg(
                    [
                        "config",
                        "--local",
                        "extensions.edensparse",
                        "" if is_edensparse_enabled else "!",
                    ],
                )

            rollbacks.append(restore_hgrc)

            if is_sparse_disabled and is_edensparse_enabled:
                # no config changes needed
                log(f"no need to configure sapling for {checkout.name}")
                return False

            log(f"configuring sapling for {checkout.name}")
            hg(["config", "--local", "extensions.sparse", "!"])

            hg(
                ["config", "--local", "extensions.edensparse", ""],
            )
            fault_injector.try_inject("unexpected_exception_after_sapling_config")
            return True

        def update_snapshot_file(
            checkout: "EdenCheckout",
            rollbacks: List[Any] = rollbacks,
            fault_injector: NaiveFaultInjector = fault_injector,
        ) -> bool:
            """
            Update snapshot file to apply the 'null' filter
            This should be done before updating the config file to switch scm_type to "filteredhg"
            because the config is needed to correctly parse the snapshot file

            Returns True when the SNAPSHOT file updated with filtered id
            Returns False when no update to SNAPSHOT file
            """
            try:
                snapshot_state = checkout.get_snapshot()
            except Exception:
                # Skip the migration for any error since we cannot attempt migration on a corrupted mount.
                # case 3 will raise so it's captured here
                return False

            snapshot_file = checkout.state_dir / SNAPSHOT
            snapshot_backup = checkout.state_dir / SNAPSHOT_BACKUP

            if snapshot_file.exists():
                with open(snapshot_file, "rb") as f:
                    write_file_atomically(snapshot_backup, f.read())

            def restore_snapshot(
                snapshot_file: Path = snapshot_file,
                snapshot_backup: Path = snapshot_backup,
            ) -> None:
                if snapshot_backup.exists():
                    with open(snapshot_backup, "rb") as f:
                        write_file_atomically(snapshot_file, f.read())
                    snapshot_backup.unlink()

            rollbacks.append(restore_snapshot)

            if snapshot_state.working_copy_parent is None:
                # case 1
                # Skip the migration for this checkout
                log(f"no need to update snapshot(case 1) file for {checkout.name}")
                return False
            elif (
                snapshot_state.working_copy_parent == snapshot_state.last_checkout_hash
            ):
                # case 2
                if snapshot_state.last_filter_id is not None:
                    # Skip the migration for this checkout
                    log(f"no need to update snapshot(case 2) file for {checkout.name}")
                    return False
                # Apply the 'null' filter
                log(f"updating snapshot file(case 2) for {checkout.name}")
                filtered_rootid = create_filtered_rootid(
                    snapshot_state.working_copy_parent, b"null"
                )
                new_snapshot_bytes = (
                    SNAPSHOT_MAGIC_2
                    + struct.pack(">L", len(filtered_rootid))
                    + filtered_rootid
                )
                write_file_atomically(snapshot_file, new_snapshot_bytes)
                fault_injector.try_inject("unexpected_exception_after_snapshot_update")
                return True
            else:
                # case 4
                working_copy_parent = snapshot_state.working_copy_parent
                checked_out_revision = snapshot_state.last_checkout_hash
                parent_filter = snapshot_state.parent_filter_id
                checked_out_filter = snapshot_state.last_filter_id
                if parent_filter is None and checked_out_filter is None:
                    log(f"updating snapshot file(case 4) for {checkout.name}")
                    # Apply the 'null' filter
                    working_copy_parent_filtered_rootid = create_filtered_rootid(
                        working_copy_parent, NULL_FILTER
                    )
                    checkout_out_filtered_rootid = create_filtered_rootid(
                        checked_out_revision, NULL_FILTER
                    )
                    encoded_working_copy_parent_filtered_rootid = (
                        struct.pack(">L", len(working_copy_parent_filtered_rootid))
                        + working_copy_parent_filtered_rootid
                    )
                    encoded_checkout_out_filtered_rootid = (
                        struct.pack(">L", len(checkout_out_filtered_rootid))
                        + checkout_out_filtered_rootid
                    )

                    # Write everything back to the snapshot file so "null" filter is applied
                    # for both working copy parent and checked out revision
                    new_snapshot_bytes = (
                        SNAPSHOT_MAGIC_4
                        + encoded_working_copy_parent_filtered_rootid
                        + encoded_checkout_out_filtered_rootid
                    )

                    write_file_atomically(snapshot_file, new_snapshot_bytes)
                    return True
                # Skip the migration for this checkout since it's already migrated
                log(f"no need to update snapshot file(case 4) for {checkout.name}")
                return False

        def create_empty_sparse_file(
            checkout: "EdenCheckout",
            rollbacks: List[Any] = rollbacks,
            fault_injector: NaiveFaultInjector = fault_injector,
        ) -> bool:
            """
            Create an empty .hg/sparse file

            Return True if the sparse file is created
            Return False if the sparse file already exists
            """
            hg_dir = checkout.hg_dot_path
            sparse_file = os.path.join(hg_dir, "sparse")
            if not os.path.exists(sparse_file):
                log(f"creating empty sparse file for {checkout.name}")
                rollbacks.append(lambda: Path(sparse_file).unlink(missing_ok=True))
                with open(sparse_file, "w") as f:
                    f.write("")
                    fault_injector.try_inject("unexpected_exception_after_sparse_file")
                    return True
            log(f"no need to create empty sparse file for {checkout.name}")
            return False

        def update_hg_requires(
            checkout: "EdenCheckout",
            rollbacks: List[Any] = rollbacks,
            fault_injector: NaiveFaultInjector = fault_injector,
        ) -> bool:
            """
            Add "edensparse" to .hg/requires

            Return True if "edensparse" is added to .hg/requires
            Return False if "edensparse" is already in .hg/requires
            """
            hg_dir = checkout.hg_dot_path
            # add "edensparse" to .hg/requires
            requires_file: str = os.path.join(hg_dir, "requires")

            # read all lines from requires file and check if "edensparse" is already present
            # if not, add "edensparse" to the end of the line list
            # sort the list and write it back to the requires file
            # Read all lines from requires file and check if "edensparse" is already present.
            # If not, add "edensparse" to the end of the line list, sort, and write back.
            with open(requires_file, "rb") as f:
                file_bytes: bytes = f.read()
            lines = file_bytes.decode("utf-8").splitlines(keepends=True)

            def write_original_lines() -> None:
                write_file_atomically(Path(requires_file), file_bytes)

            rollbacks.append(write_original_lines)
            lines_clean = [line.strip() for line in lines]
            if "edensparse" not in lines_clean:
                log(f"updating hg requires file for {checkout.name}")
                lines_clean.append("edensparse")
                lines_clean = sorted(set(lines_clean))
                # Write back with newline after each entry
                with open(requires_file, "w") as f:
                    for line in lines_clean:
                        f.write(f"{line}\n")
                    fault_injector.try_inject(
                        "unexpected_exception_after_requires_file"
                    )
                    return True
            log(f"no need to update hg requires file for {checkout.name}")
            return False

        def update_config_toml_file(
            checkout: "EdenCheckout",
            rollbacks: List[Any] = rollbacks,
            fault_injector: NaiveFaultInjector = fault_injector,
        ) -> None:
            config_file = checkout.state_dir / MOUNT_CONFIG
            config_backup = checkout.state_dir / CONFIG_TOML_BACKUP

            if config_file.exists():
                with open(config_file, "rb") as f:
                    write_file_atomically(config_backup, f.read())

            def restore_config_toml(
                config_file: Path = config_file,
                config_backup: Path = config_backup,
            ) -> None:
                if config_backup.exists():
                    with open(config_backup, "rb") as f:
                        write_file_atomically(config_file, f.read())
                    config_backup.unlink()

            rollbacks.append(restore_config_toml)

            config = checkout.get_config()  # config.toml
            if config.scm_type == "filteredhg":
                log(f"no need to update config toml file for {checkout.name}")
                return

            log(f"updating config toml file for {checkout.name}")
            config = config._replace(scm_type="filteredhg")
            checkout.save_config(config)
            fault_injector.try_inject("unexpected_exception_after_config_toml")

        try:
            if step == EdensparseMigrationStep.PRE_EDEN_START:
                if not update_snapshot_file(checkout):
                    # the migration should return early here if we are in the progress of
                    # a checkout.
                    # the migration will be retried next time when edenfs is started
                    log(
                        f"snapshot_file not updated: migration skipped for {checkout.path.name}"
                    )
                    continue
                update_config_toml_file(checkout)
            else:
                config = checkout.get_config()
                if config.scm_type != "filteredhg":
                    log(
                        f"scm_type not updated: migration skipped for {checkout.path.name}"
                    )
                    continue
                snapshot_state = checkout.get_snapshot()
                if snapshot_state.last_filter_id is None:
                    # SNAPSHOT file not updated
                    # skip this part of migration and retry next time
                    log(
                        f"snapshot_file not updated: migration skipped for {checkout.path.name}"
                    )
                    continue

                # The following steps require the checkout to be mounted so we can access the
                # files under .hg folder
                # We wait for some time if the checkout is not mounted yet and claim a failure
                # if we hit the timeout waiting
                try:
                    mnt_timeout = int(
                        os.environ.get("EDENSPARSE_MIGRATION_MOUNT_TIMEOUT", 5)
                    )
                except ValueError:
                    mnt_timeout = 5

                log(
                    f"waiting for {mnt_timeout} seconds before {checkout.path.name} is mounted..."
                )

                if not checkout.wait_until_mounted(mnt_timeout):
                    raise TimeoutError(
                        f"failed to mount {checkout.path.name} after {mnt_timeout} seconds"
                    )

                if any(
                    [
                        create_empty_sparse_file(checkout),
                        update_hg_requires(checkout),
                        configure_sapling(checkout),
                    ]
                ):
                    marker_file = os.path.join(checkout.hg_dot_path, MIGRATION_MARKER)
                    open(marker_file, "a").close()
                    log(f"migration complete for {checkout.path.name}")
                    num_successful_migration += 1

                    # Clean up Step 1 backups after successful migration
                    for backup_name in [SNAPSHOT_BACKUP, CONFIG_TOML_BACKUP]:
                        backup_path = checkout.state_dir / backup_name
                        if backup_path.exists():
                            backup_path.unlink()
                else:
                    log(f"no changes made, migration skipped for {checkout.path.name}")
        except Exception as e:
            print("edensparse migration failed: ", e, file=sys.stderr)
            # log checkout info and exception
            import traceback

            migration_exceptions.append(
                f"Migration exception: {checkout.path}\n{traceback.format_exc()}"
            )
            print("rolling back changes...", file=sys.stderr)

            if step == EdensparseMigrationStep.POST_EDEN_START:
                # Restore Step 1 backups if they exist (for cross-step rollback)
                # This handles the case where POST_EDEN_START fails and we need to
                # rollback changes made in PRE_EDEN_START (whose rollback functions
                # are no longer available since they were in a different function call)
                snapshot_backup = checkout.state_dir / SNAPSHOT_BACKUP
                config_backup = checkout.state_dir / CONFIG_TOML_BACKUP
                if snapshot_backup.exists() or config_backup.exists():
                    log(f"restoring step 1 backups for {checkout.path.name}")
                    try:
                        if snapshot_backup.exists():
                            with open(snapshot_backup, "rb") as f:
                                write_file_atomically(
                                    checkout.state_dir / SNAPSHOT, f.read()
                                )
                            snapshot_backup.unlink()
                        if config_backup.exists():
                            with open(config_backup, "rb") as f:
                                write_file_atomically(
                                    checkout.state_dir / MOUNT_CONFIG, f.read()
                                )
                            config_backup.unlink()
                        # Unmount and remount the checkout to apply the restored files
                        try:
                            log(f"unmounting {checkout.path} to apply restored config")
                            instance.unmount(str(checkout.path))
                            log(f"remounting {checkout.path}")
                            instance.mount(checkout.path, read_only=False)
                        except Exception as mount_err:
                            print(
                                f"failed to unmount/remount {checkout.path}: {mount_err}. "
                                "The restored files will take effect on next edenfs restart.",
                                file=sys.stderr,
                            )
                    except Exception as restore_err:
                        print(
                            f"failed to restore step 1 backups: {restore_err}",
                            file=sys.stderr,
                        )

            # If step == EdensparseMigrationStep.PRE_EDEN_START, the operations above are included
            # in the rollback functions, so we just go through the rollback functions
            for rollback in rollbacks[::-1]:
                try:
                    rollback()
                except Exception as e:
                    print(
                        "rollback for edensparse migration failed: ", e, file=sys.stderr
                    )

    # Only log when:
    # 1. there are exceptions from any steps
    # 2. at least one migration is successful
    #
    # Do not log when:
    # 1. no checkouts are migrated (because they are already filteredfs)
    if migration_exceptions or num_successful_migration:
        instance.log_sample(
            "edensparse_migration",
            success=len(migration_exceptions) == 0,
            migration_count=num_successful_migration,
            exception="\n".join([e_str for e_str in migration_exceptions]),
        )


class NaiveFaultInjector:
    """
    A naive fault injector that injects faults by raising exceptions when needed.

    Injector knows when to raise an exception by checking if a file with
    the specified key exists in the eden client state directory.

    Ideally this should only be used in tests:
    ```
    fault_key = "my_fault_key"
    with NaiveFaultInjector(eden_client_state_dir) as fault_injector:
        fault_injector.register_test_only_fault(fault_key)
    ```
    """

    def __init__(self, path: Path) -> None:
        self.eden_client_dir = path
        self.prefix = "TEST_ONLY_FILE_FROM_FAULT_INJECTOR"

    def try_inject(self, key: str) -> None:
        """
        Checks if the file at self.path / key exists.
        If it does, raises a RuntimeError.
        """
        fault_file = self.eden_client_dir / self._file_name(key)
        if fault_file.exists():
            raise RuntimeError(f"Test-Only Fault Injected: key={key}")

    def register_test_only_fault(self, key: str) -> None:
        """
        Creates a file at self.path / key to register a fault injection point.
        """
        fault_file = self.eden_client_dir / self._file_name(key)
        fault_file.touch(exist_ok=True)

    def _file_name(self, key: str) -> str:
        return self.prefix + key.upper()

    def clean(self) -> None:
        """
        Removes testing files in self.eden_client_dir that were registered as fault keys.
        """
        if not self.eden_client_dir.exists():
            return

        for file in self.eden_client_dir.iterdir():
            if file.is_file() and file.name.startswith(self.prefix):
                try:
                    file.unlink()
                except Exception:
                    pass

    def __enter__(self) -> "NaiveFaultInjector":
        return self

    # pyre-fixme[2]: Parameter must be annotated.
    def __exit__(self, exc_type, exc_val, exc_tb) -> bool:
        self.clean()
        # Do not suppress exceptions
        return False
