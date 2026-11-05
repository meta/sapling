#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import binascii
import collections
import datetime
import errno
import functools
import json
import logging
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import time
import typing
import uuid
from contextlib import contextmanager
from enum import Enum
from pathlib import Path
from typing import (
    Any,
    Callable,
    Dict,
    Generator,
    IO,
    KeysView,
    List,
    Mapping,
    Optional,
    Set,
    Tuple,
    Union,
)

import toml
from eden.fs.service.eden.thrift_clients import EdenService
from eden.fs.service.eden.thrift_types import (
    EdenError,
    MountArgument,
    MountId,
    MountInfo,
    MountState,
    UnmountArgument,
)
from eden.thrift import client
from eden.thrift.client import EdenNotRunningError
from filelock import BaseFileLock, FileLock
from thrift.python.exceptions import TransportError

from . import configinterpolator, configutil, telemetry, util, version

if sys.platform == "win32":
    from .file_handler_tools import WinFileHandlerReleaser
from .util import (
    CLONE_SUCCEEDED,
    FUSE_MOUNT_PROTOCOL_STRING,
    HealthStatus,
    INTENTIONALLY_UNMOUNTED,
    MOUNT_CONFIG,
    NFS_MOUNT_PROTOCOL_STRING,
    print_stderr,
    PRJFS_MOUNT_PROTOCOL_STRING,
    SNAPSHOT,
    SNAPSHOT_MAGIC_1,
    SNAPSHOT_MAGIC_2,
    SNAPSHOT_MAGIC_3,
    SNAPSHOT_MAGIC_4,
    Spinner,
    write_file_atomically,
)

log: logging.Logger = logging.getLogger(__name__)

if typing.TYPE_CHECKING:
    from eden.fs.cli.redirect import RedirectionType  # noqa: F401

# Use --etcEdenDir to change the value used for a given invocation
# of the eden cli.
if sys.platform == "win32":
    DEFAULT_ETC_EDEN_DIR = "C:\\ProgramData\\facebook\\eden"
else:
    DEFAULT_ETC_EDEN_DIR = "/etc/eden"

# These are INI files that hold config data.
# CONFIG_DOT_D is relative to DEFAULT_ETC_EDEN_DIR, or whatever the
# effective value is for that path
CONFIG_DOT_D = "config.d"
# USER_CONFIG is relative to the HOME dir for the user
USER_CONFIG = ".edenrc"
# SYSTEM_CONFIG and DYNAMIC_CONFIG are relative to the etc eden dir
SYSTEM_CONFIG = "edenfs.rc"
DYNAMIC_CONFIG = "edenfs_dynamic.rc"

# These paths are relative to the user's client directory.
CLIENTS_DIR = "clients"
CONFIG_JSON = "config.json"
CONFIG_JSON_LOCK = "config.json.lock"

# List of supported repository types. This should stay in sync with the list
# in the Rust CLI at fs/cli_rs/edenfs-client/src/checkout.rs and the list in
# the Daemon's CheckoutConfig at fs/config/CheckoutConfig.h.
DEFAULT_REVISION = {  # supported repo name -> default bookmark
    # The daemon no longer mounts "git" checkouts; the CLI keeps recognising
    # the type so existing ones can still be listed and removed.
    "git": "refs/heads/master",
    "hg": "first(present(master) + .)",
    "filteredhg": "first(present(master) + .)",
    "recas": "",
    "http": "",
}

HG_REPO_TYPES = ["hg", "filteredhg"]

SUPPORTED_REPOS: KeysView[str] = DEFAULT_REVISION.keys()

SUPPORTED_MOUNT_PROTOCOLS: Set[str] = {
    FUSE_MOUNT_PROTOCOL_STRING,
    NFS_MOUNT_PROTOCOL_STRING,
    PRJFS_MOUNT_PROTOCOL_STRING,
}

SUPPORTED_INODE_CATALOG_TYPES: Set[str] = {
    "legacy",
    "inmemory",
    "legacyephemeral",
}

# Create a readme file with this name in the mount point directory.
# The intention is for this to contain instructions telling users what to do if their
# EdenFS mount is not currently mounted.
NOT_MOUNTED_README_PATH = "README_EDEN.txt"
# The path under /etc/eden where site-specific contents for the not-mounted README can
# be found.
NOT_MOUNTED_SITE_SPECIFIC_README_PATH = "NOT_MOUNTED_README.txt"
# The default contents for the not-mounted README if a site-specific template
# is not found.
NOT_MOUNTED_DEFAULT_TEXT = """\
This directory is the mount point for a virtual checkout managed by EdenFS.

If you are seeing this file that means that your repository checkout is not
currently mounted.  This could either be because the edenfs daemon is not
currently running, or it simply does not have this checkout mounted yet.

You can run "eden doctor" to check for problems with EdenFS and try to have it
automatically remount your checkouts.
"""

# Default Thrift client timeout values (in seconds). Some of these might be
# overridden or turned off by configs.
MOUNT_TIMEOUT_SECONDS = 30
UNMOUNT_TIMEOUT_SECONDS = 60


class CheckoutPathProblemType(Enum):
    NESTED_CHECKOUT = "nested_checkout"
    INSIDE_BACKING_REPO = "inside_backing_repo"
    NONE = None


class UsageError(Exception):
    pass


class FileError(Exception):
    pass


class InProgressCheckoutError(Exception):
    from_commit: str
    to_commit: str
    pid: int

    def __init__(self, from_commit: str, to_commit: str, pid: int) -> None:
        super().__init__()
        self.from_commit = from_commit
        self.to_commit = to_commit
        self.pid = pid

    def __str__(self) -> str:
        return (
            f"A checkout operation is ongoing: {self.from_commit} -> {self.to_commit}"
        )


class CheckoutConfigCorruptedError(Exception):
    pass


class CheckoutConfig(typing.NamedTuple):
    """Configuration for an EdenFS checkout. A checkout stores its config in config.toml
    it its state directory (.eden/clients/<checkout_name>/config.toml)

    - backing_repo: The path where the true repo resides on disk.  For mercurial backing
        repositories this does not include the final ".hg" directory component.
    - scm_type: "hg", "filteredhg", or "git"
    - mount_protocol: "fuse", "nfs" or "prjfs"
    - case_sensitive: whether the mount point is case sensitive. Default to
      false on Windows and macOS.
    - require_utf8_path: whether the mount point will disallow non-utf8 paths
      to be written to it.
    - guid: Used on Windows by ProjectedFS to identify this checkout.
    - redirections: dict where keys are relative pathnames in the EdenFS mount
      and the values are RedirectionType enum values that describe the type of
      the redirection.
    - redirection_targets: dict where keys are relative pathnames in the EdenFS mount
      and the values are target path for corresponding symlink.
    """

    backing_repo: Path
    scm_type: str
    guid: str
    mount_protocol: str
    case_sensitive: bool
    require_utf8_path: bool
    default_revision: str
    redirections: Dict[str, "RedirectionType"]
    redirection_targets: Dict[str, str]
    active_prefetch_profiles: List[str]
    use_write_back_cache: bool
    re_use_case: str
    inode_catalog_type: Optional[str]
    off_mount_repo_dir: bool


class ListMountInfo(typing.NamedTuple):
    path: Path
    data_dir: Path
    state: Optional[MountState]
    configured: bool
    backing_repo: Optional[Path]
    fs_channel_type: Optional[str] = None
    fuse_transport: Optional[str] = None
    nfs_transport: Optional[str] = None
    visible_in_daemon_namespace: Optional[bool] = None

    def to_json_dict(self) -> Dict[str, Any]:
        if self.state is None:
            state_str = "NOT_RUNNING"
        elif hasattr(self.state, "name"):
            state_str = self.state.name
        else:
            # State is a raw int
            state_str = MountState(self.state).name
        d: Dict[str, Any] = {
            "data_dir": self.data_dir.as_posix(),
            "state": state_str,
            "configured": self.configured,
            "backing_repo": (
                self.backing_repo.as_posix() if self.backing_repo is not None else None
            ),
        }
        if self.fs_channel_type is not None:
            d["fs_channel_type"] = self.fs_channel_type
        if self.fuse_transport is not None:
            d["fuse_transport"] = self.fuse_transport
        if self.nfs_transport is not None:
            d["nfs_transport"] = self.nfs_transport
        if self.visible_in_daemon_namespace is not None:
            d["visible_in_daemon_namespace"] = self.visible_in_daemon_namespace
        return d


class SnapshotState(typing.NamedTuple):
    working_copy_parent: str
    last_checkout_hash: str
    parent_filter_id: Optional[bytes] = None
    last_filter_id: Optional[bytes] = None


class AbstractEdenInstance:
    def get_config_int(self, key: str, default: int) -> int:
        val = self.get_config_value(key, str(default))
        try:
            return int(val)
        except ValueError:
            return default

    def get_config_value(self, key: str, default: str) -> str: ...

    def get_config_bool(self, key: str, default: bool) -> bool: ...

    def get_config_strs(
        self, key: str, default: configutil.Strs
    ) -> configutil.Strs: ...

    def get_checkouts(self) -> List["EdenCheckout"]: ...

    def get_mounts(self) -> Dict[Path, ListMountInfo]: ...


def _remove_client_dir(client_dir: Path) -> None:
    # EdenFS does not mount a client directory that has no SNAPSHOT.
    # edenfsctl reads config.toml to remove a partly deleted checkout.
    (client_dir / SNAPSHOT).unlink(missing_ok=True)
    for entry in client_dir.iterdir():
        if entry.name == MOUNT_CONFIG:
            continue
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry)
        else:
            entry.unlink()
    (client_dir / MOUNT_CONFIG).unlink(missing_ok=True)
    client_dir.rmdir()


class EdenInstance(AbstractEdenInstance):
    """This class contains information about a particular edenfs instance.

    It provides APIs for communicating with edenfs over thrift and for examining and
    modifying the list of checkouts managed by this edenfs instance.
    """

    _telemetry_logger: Optional[telemetry.TelemetryLogger] = None
    _home_dir: Path
    _user_config_path: Path
    _dynamic_config_path: Path
    _system_config_path: Path
    _config_dir: Path

    def __init__(
        self,
        config_dir: Union[Path, str, None],
        etc_eden_dir: Union[Path, str, None],
        home_dir: Union[Path, str, None],
        interpolate_dict: Optional[Dict[str, str]] = None,
    ) -> None:
        self._etc_eden_dir = Path(etc_eden_dir or DEFAULT_ETC_EDEN_DIR)
        self._home_dir = Path(home_dir) if home_dir is not None else util.get_home_dir()
        self._user_config_path = self._home_dir / USER_CONFIG
        self._dynamic_config_path = self._etc_eden_dir / DYNAMIC_CONFIG
        self._system_config_path = self._etc_eden_dir / SYSTEM_CONFIG
        self._interpolate_dict = interpolate_dict

        # We call resolve() to resolve any symlinks in the config directory location.
        # This is particularly important when starting edenfs, since edenfs in some
        # cases will try to access this path as root (e.g., when creating bind mounts).
        # In some cases this path may traverse symlinks that are readable as the
        # original user but not as root: this can happen if the user has a home
        # directory on NFS, which may not be readable as root.
        if config_dir:
            self._config_dir = Path(config_dir)
        else:
            if sys.platform == "win32":
                config_dir_default = self._home_dir / ".eden"
            else:
                config_dir_default = self._home_dir / "local" / ".eden"
            self._config_dir = Path(
                self.get_config_value(
                    "core.edenDirectory", default=str(config_dir_default)
                )
            )

        self._config_dir = self._config_dir.resolve(strict=False)

    def __repr__(self) -> str:
        return f"EdenInstance({self._config_dir!r})"

    @property
    def state_dir(self) -> Path:
        return self._config_dir

    @property
    def etc_eden_dir(self) -> Path:
        return self._etc_eden_dir

    @property
    def home_dir(self) -> Path:
        return self._home_dir

    @property
    def user_config_path(self) -> Path:
        return self._user_config_path

    def read_configs(self, paths: List[Path]) -> configutil.EdenConfigParser:
        """
        reads all files specified in paths and parses the configs,
        skips any files which are not found
        """
        parser = configutil.EdenConfigParser(
            interpolation=configinterpolator.EdenConfigInterpolator(
                self._config_variables
            )
        )
        for path in paths:
            try:
                toml_cfg = load_toml_config(path)
            except FileNotFoundError:
                # Ignore missing config files. Eg. user_config_path is optional
                continue
            except FileError as e:
                log.warning(f"Not reading {path}: {str(e)}")
                continue
            parser.read_dict(toml_cfg)
        return parser

    def _loadConfig(self) -> configutil.EdenConfigParser:
        """to facilitate templatizing a centrally deployed config, we
        allow a limited set of env vars to be expanded.
        ${HOME} will be replaced by the user's home dir,
        ${USER} will be replaced by the user's login name.
        These are coupled with the equivalent code in
        eden/fs/config/CheckoutConfig.cpp and must be kept in sync.
        """
        return self.read_configs(self.get_rc_files())

    @property
    def _config_variables(self) -> Dict[str, str]:
        if sys.platform == "win32":
            # We don't have user ids on Windows right now.
            # We should update this code if and when we add user id support.
            user_id = 0
            user_name = "USERNAME"
        else:
            user_id = os.getuid()
            user_name = "USER"

        interpolate_dict = self._interpolate_dict
        if interpolate_dict is not None:
            return interpolate_dict
        else:
            return {
                "USER": os.environ.get(user_name, ""),
                "USER_ID": str(user_id),
                "HOME": str(self._home_dir),
            }

    def get_rc_files(self) -> List[Path]:
        result: List[Path] = []
        config_d = self._etc_eden_dir / CONFIG_DOT_D
        try:
            rc_entries = os.listdir(config_d)
        except OSError as ex:
            if ex.errno != errno.ENOENT:
                raise
            rc_entries = []

        for name in rc_entries:
            if not name.startswith(".") and name.endswith(".toml"):
                result.append(config_d / name)
        result.sort()
        result.append(self._system_config_path)
        result.append(self._dynamic_config_path)
        result.append(self._user_config_path)
        return result

    def get_telemetry_logger(self) -> telemetry.TelemetryLogger:
        logger = self._telemetry_logger
        if logger is None:
            logger = self._create_telemetry_logger()
            self._telemetry_logger = logger
        return logger

    def _create_telemetry_logger(self) -> telemetry.TelemetryLogger:
        if telemetry.telemetry_disabled_by_env():
            return telemetry.NullTelemetryLogger()

        try:
            # pyre-fixme [21]: Undefined import Could not find a module corresponding to import
            from eden.fs.cli.facebook.xplat_logger import get_xplat_logger  # @manual

            # Python CLI telemetry continues to use edenfs_events. Other
            # destinations request their own view from the same transport.
            return get_xplat_logger()
        except ImportError:
            # OSS / non-internal builds do not ship XplatLogger; use the legacy loggers.
            pass
        except Exception as ex:
            log.warning(f"XplatLogger construction failed, falling back: {ex}")

        try:
            # pyre-fixme [21]: Undefined import Could not find a module corresponding to import
            from eden.fs.cli.facebook import scuba_telemetry  # @manual

            # pyre-fixme [16]: Undefined attribute
            return scuba_telemetry.ScubaTelemetryLogger()
        except (ImportError, NotImplementedError):
            pass

        scribe_cat = self.get_config_value("telemetry.scribe-cat", default="")
        scribe_category = self.get_config_value("telemetry.scribe-category", default="")
        if scribe_cat == "" or scribe_category == "":
            return telemetry.NullTelemetryLogger()
        return telemetry.ExternalTelemetryLogger([scribe_cat, scribe_category])

    def build_sample(
        self, log_type: str, **kwargs: Union[bool, int, str, float, Set[str]]
    ) -> telemetry.TelemetrySample:
        return self.get_telemetry_logger().new_sample(log_type, **kwargs)

    def log_sample(self, log_type: str, **kwargs: telemetry.TelemetryTypes) -> None:
        self.get_telemetry_logger().log(log_type, **kwargs)

    def get_known_bad_edenfs_versions(self) -> Dict[str, List[str]]:
        """
        Get a dictionary mapping bad EdenFS versions to their reasons.
        """
        # 'bad_versions_config' format: [<bad_version_1>|<sev_1(optional):reason_1>,<bad_version_2>|<sev_2(optional):reason_2>]
        bad_versions_config = self.get_config_strs(
            "doctor.known-bad-edenfs-versions", default=configutil.Strs()
        )
        if not bad_versions_config:
            return {}
        bad_versions_map = {}
        for item in bad_versions_config:
            if "|" not in item:
                log.warning(
                    f"`known-bad-edenfs-versions` config has an invalid entry `{item}`.",
                )
                continue
            version, reason = item.split("|", 1)
            version = version.strip()
            reason = reason.strip()
            if version in bad_versions_map:
                bad_versions_map[version].append(reason)
            else:
                bad_versions_map[version] = [reason]
        return bad_versions_map

    def get_running_version_parts(self) -> Tuple[str, str]:
        """Get a tuple containing (version, release) of the currently running EdenFS
        daemon.

        The version and release strings will both be the empty string if a development
        build of EdenFS is being used.

        Throws an EdenNotRunningError if EdenFS does not currently appear to be running.
        """
        bi = self.get_server_build_info()
        return (
            bi.get("build_package_version", ""),
            bi.get("build_package_release", ""),
        )

    def get_current_and_running_versions(self) -> Tuple[str, Optional[str]]:
        try:
            running = self.get_running_version()
        except EdenNotRunningError:
            # return None if EdenFS does not currently appear to be running
            running = None
        return version.get_current_version(), running

    def get_running_version(self) -> str:
        """Get a human-readable string representation of the currently running EdenFS
        version.

        Will return the string "-" if a dev build of EdenFS is being used.

        Throws an EdenNotRunningError if EdenFS does not currently appear to be running.
        """
        return version.format_eden_version(self.get_running_version_parts())

    def get_config_value(self, key: str, default: str) -> str:
        parser = self._loadConfig()
        section, option = key.split(".", 1)
        return parser.get_str(section, option, default=default)

    def get_config_bool(self, key: str, default: bool) -> bool:
        parser = self._loadConfig()
        section, option = key.split(".", 1)
        return parser.get_bool(section, option, default=default)

    def get_config_strs(self, key: str, default: configutil.Strs) -> configutil.Strs:
        parser = self._loadConfig()
        section, option = key.split(".", 1)
        return parser.get_strs(section, option, default=default)

    def print_full_config(self, out: IO[bytes]) -> None:
        parser = self._loadConfig()
        data: Dict[str, Mapping[str, str]] = {}
        for section in parser.sections():
            data[section] = parser.get_section_str_to_any(section)
        out.write(toml.dumps(data).encode())

    def get_mount_paths(self) -> List[str]:
        """Return the paths of the set mount points stored in config.json"""
        return [str(path) for path in self._get_directory_map().keys()]

    @contextmanager
    def get_thrift_client(
        self, timeout: Optional[float] = None
    ) -> Generator[EdenService.Sync, None, None]:
        with client.create_thrift_client(
            eden_dir=str(self._config_dir),
            timeout=timeout if timeout is not None else 0,
        ) as thrift_client:
            yield thrift_client

    def get_checkout_info(
        self, path: Union[Path, str]
    ) -> "collections.OrderedDict[str, Union[str, bool]]":
        """
        Given a path to a checkout, return a dictionary containing diagnostic
        information about it.
        """
        path = Path(path).resolve(strict=False)
        client_dir = self._get_client_dir_for_mount_point(path)
        checkout = EdenCheckout(self, path, client_dir)
        return self.get_checkout_info_from_checkout(checkout)

    def get_checkout_info_from_checkout(
        self, checkout: "EdenCheckout"
    ) -> "collections.OrderedDict[str, Union[str, bool]]":
        checkout_config = checkout.get_config()

        error = None
        snapshot = None
        try:
            snapshot = checkout.get_snapshot()
        except Exception as ex:
            error = ex

        ret = collections.OrderedDict(
            [
                ("mount", str(checkout.path)),
                ("scm_type", checkout_config.scm_type),
                ("state_dir", str(checkout.state_dir)),
                ("mount_protocol", checkout_config.mount_protocol),
                ("case_sensitive", checkout_config.case_sensitive),
                ("backing_repo", str(checkout.get_backing_repo_path())),
                ("home_dir", str(self.home_dir)),
            ]
        )

        if checkout_config.inode_catalog_type is not None:
            ret["inode_catalog_type"] = checkout_config.inode_catalog_type

        if snapshot is not None:
            ret["checked_out_revision"] = snapshot.last_checkout_hash
            ret["working_copy_parent"] = snapshot.working_copy_parent
        if error is not None:
            ret["error"] = str(error)

        return ret

    def get_mounts(self) -> Dict[Path, ListMountInfo]:
        try:
            with self.get_thrift_client() as client:
                thrift_mounts: List[MountInfo] = list(client.listMounts())
        except (EdenNotRunningError, TransportError):
            thrift_mounts = []

        config_mounts = self.get_checkouts()
        return self._combine_mount_info(thrift_mounts, config_mounts)

    @classmethod
    def _combine_mount_info(
        cls,
        thrift_mounts: List[MountInfo],
        config_checkouts: List["EdenCheckout"],
    ) -> Dict[Path, ListMountInfo]:
        mount_points: Dict[Path, ListMountInfo] = {}

        for thrift_mount in thrift_mounts:
            path = Path(os.fsdecode(thrift_mount.mountPoint))
            state = thrift_mount.state
            data_dir = Path(os.fsdecode(thrift_mount.edenClientPath))

            # this line is for pyre :(
            raw_backing_repo = thrift_mount.backingRepoPath
            backing_repo = (
                Path(os.fsdecode(raw_backing_repo))
                if raw_backing_repo is not None
                else None
            )

            mount_points[path] = ListMountInfo(
                path=path,
                data_dir=data_dir,
                state=state,
                configured=False,
                backing_repo=backing_repo,
                fs_channel_type=thrift_mount.fsChannelType,
                fuse_transport=thrift_mount.fuseTransport,
                nfs_transport=thrift_mount.nfsTransport,
                visible_in_daemon_namespace=thrift_mount.visibleInDaemonNamespace,
            )

        # Add all mount points listed in the config that were not reported
        # in the thrift call.
        for checkout in config_checkouts:
            mount_info = mount_points.get(checkout.path, None)
            checkout_config: Optional[CheckoutConfig] = None
            try:
                if mount_info is None or mount_info.backing_repo is None:
                    checkout_config = checkout.get_config()
            except CheckoutConfigCorruptedError as ex:
                if isinstance(ex.__cause__, FileNotFoundError):
                    log.warning(
                        "Skipping configured checkout with missing client state: %s (%s)",
                        checkout.path,
                        checkout.state_dir,
                    )
                    continue
                raise

            if mount_info is not None:
                if mount_info.backing_repo is None:
                    assert checkout_config is not None
                    mount_info = mount_info._replace(
                        backing_repo=checkout_config.backing_repo
                    )
                mount_points[checkout.path] = mount_info._replace(configured=True)
            else:
                assert checkout_config is not None
                mount_points[checkout.path] = ListMountInfo(
                    path=checkout.path,
                    data_dir=checkout.state_dir,
                    state=None,
                    configured=True,
                    backing_repo=checkout_config.backing_repo,
                )

        return mount_points

    def clone(
        self,
        checkout_config: CheckoutConfig,
        path: str,
        snapshot_id: str,
        filter_paths: Optional[List[str]] = None,
    ) -> None:
        if Path(path) in self._get_directory_map():
            raise Exception(
                """\
mount path %s is already configured (see `eden list`). \
Do you want to run `eden mount %s` instead?"""
                % (path, path)
            )

        # Create the mount point directory
        self._create_mount_point_dir(path)

        # Create client directory
        clients_dir = self._get_clients_dir()
        clients_dir.mkdir(parents=True, exist_ok=True)
        client_dir = self._create_client_dir_for_path(clients_dir, path)

        # Store snapshot ID
        checkout = EdenCheckout(self, Path(path), Path(client_dir))
        if snapshot_id:
            if checkout_config.scm_type == "filteredhg":
                filter_id = (
                    self._get_filter_id(snapshot_id, filter_paths, checkout_config)
                    or b"null"
                )
                filtered_root_id = util.create_filtered_rootid(snapshot_id, filter_id)
                checkout.save_snapshot(filtered_root_id)
            else:
                checkout.save_snapshot(snapshot_id.encode())
        else:
            raise Exception("snapshot id not provided")

        checkout.save_config(checkout_config)

        # Prepare to mount
        mount_info = MountArgument(
            mountPoint=os.fsencode(path),
            edenClientPath=os.fsencode(client_dir),
            readOnly=False,
        )

        mount_timeout = self.get_config_int(
            "clone.mount-timeout", default=MOUNT_TIMEOUT_SECONDS
        )
        # Treat a value of zero as no timeout
        if mount_timeout == 0:
            mount_timeout = None

        with self.get_thrift_client(timeout=mount_timeout) as client:
            client.mount(mount_info)

        self._post_clone_checkout_setup(checkout, snapshot_id, filter_paths)

        # Add mapping of mount path to client directory in config.json
        self._add_path_to_directory_map(Path(path), os.path.basename(client_dir))

    def _create_mount_point_dir(self, path: str) -> None:
        # Create the directory
        try:
            os.makedirs(path)
        except OSError as e:
            if e.errno != errno.EEXIST:
                raise

            # If the path already exists, make sure it is an empty directory.
            # listdir() will throw its own error if the path is not a directory.
            if len(os.listdir(path)) > 0:
                raise Exception(
                    f'The directory "{path}" already exist on disk. Use `eden rm` if this is an old EdenFS clone to remove it.'
                )

        # On non-Windows platforms, put a README file in the mount point directory.
        # This will be visible to users when the EdenFS checkout is not mounted,
        # and will contain instructions for how to get the checkout re-mounted.
        #
        # On Windows anything we put in this directory will be visible in the checkout
        # itself, so we don't want to put a README file here.
        if sys.platform != "win32":
            self._create_checkout_readme_file(path)

    def _create_checkout_readme_file(self, path: str) -> None:
        help_path = Path(path) / NOT_MOUNTED_README_PATH
        site_readme_path = self._etc_eden_dir / NOT_MOUNTED_SITE_SPECIFIC_README_PATH
        help_contents: Optional[str] = NOT_MOUNTED_DEFAULT_TEXT
        try:
            # Create a symlink to the site-specific readme file.  This helps ensure that
            # users will see up-to-date contents if the site-specific file is updated
            # later.
            with site_readme_path.open("r") as f:
                try:
                    help_path.symlink_to(site_readme_path)
                    help_contents = None
                except OSError as ex:
                    # EPERM can indicate that the underlying filesystem does not support
                    # symlinks.  Read the contents from the site-specific file in this
                    # case.  We will copy them into the file instead of making a
                    # symlink.
                    if ex.errno == errno.EPERM:
                        help_contents = f.read()
                    else:
                        raise
        except OSError as ex:
            if ex.errno == errno.ENOENT:
                # If the site-specific readme file does not exist use default contents
                help_contents = NOT_MOUNTED_DEFAULT_TEXT
            else:
                raise

        if help_contents is not None:
            with help_path.open("w") as f:
                f.write(help_contents)
                if sys.platform != "win32":
                    os.fchmod(f.fileno(), 0o444)

    def _create_client_dir_for_path(self, clients_dir: Path, path: str) -> Path:
        """Tries to create a new subdirectory of clients_dir based on the
        basename of the specified path. Tries appending an increasing sequence
        of integers to the basename if there is a collision until it finds an
        available directory name.
        """
        basename = os.path.basename(path)
        if basename == "":
            raise Exception("Suspicious attempt to clone into: %s" % path)

        i = 0
        while True:
            if i == 0:
                dir_name = basename
            else:
                dir_name = f"{basename}-{i}"

            client_dir = clients_dir / dir_name
            try:
                client_dir.mkdir()
                return client_dir
            except OSError as e:
                if e.errno == errno.EEXIST:
                    # A directory with the specified name already exists: try
                    # again with the next candidate name.
                    i += 1
                    continue
                raise

    def _get_filter_id(
        self,
        commit_id: str,
        filter_paths: Optional[List[str]],
        config: CheckoutConfig,
    ) -> Optional[bytes]:
        if filter_paths is None or config.scm_type not in HG_REPO_TYPES:
            return None
        else:
            from . import hg_util

            return hg_util.get_filter_id(commit_id, filter_paths, config.backing_repo)

    def _post_clone_checkout_setup(
        self,
        checkout: "EdenCheckout",
        commit_id: str,
        filter_paths: Optional[List[str]] = None,
    ) -> None:
        # First, check to see if the post-clone setup has been run successfully
        # before.
        clone_success_path = checkout.state_dir / CLONE_SUCCEEDED
        is_initial_mount = not clone_success_path.is_file()
        if is_initial_mount and checkout.get_config().scm_type in HG_REPO_TYPES:
            from . import hg_util

            hg_util.setup_hg_dir(checkout, commit_id, filter_paths)

        clone_success_path.touch()

        if checkout.get_config().scm_type in HG_REPO_TYPES:
            env = os.environ.copy()
            # These are set by the par machinery and interfere with Mercurial's
            # own dynamic library loading.
            env.pop("DYLD_INSERT_LIBRARIES", None)
            env.pop("DYLD_LIBRARY_PATH", None)

            subprocess.check_call(
                [
                    os.environ.get("EDEN_HG_BINARY", "hg"),
                    "debugedenrunpostupdatehook",
                    "-R",
                    str(checkout.path),
                ],
                env=env,
            )

            configs = {}
            if checkout.get_config().scm_type == "filteredhg":
                configs["extensions.edensparse"] = ""
                configs["extensions.sparse"] = "!"
            if len(configs) > 0:
                args = [f"{k}={v}" for k, v in configs.items()]
                subprocess.check_call(
                    [
                        os.environ.get("EDEN_HG_BINARY", "hg"),
                        "config",
                        "--local",
                        *args,
                        "-R",
                        str(checkout.path),
                    ],
                    env=env,
                )

    def mount(self, path: Union[Path, str], read_only: bool) -> int:
        # Load the config info for this client, to make sure we
        # know about the client.
        path = Path(path).resolve(strict=False)
        client_dir = self._get_client_dir_for_mount_point(path)
        checkout = EdenCheckout(self, path, client_dir)

        # Call checkout.get_config() for the side-effect of it raising an
        # Exception if the config is in an invalid state.
        checkout.get_config()

        # Make sure the mount path exists
        path.mkdir(parents=True, exist_ok=True)

        # Check if it is already mounted.
        try:
            root = path / ".eden" / "root"
            target = os.readlink(root)
            if Path(target) == path:
                print_stderr(
                    f"ERROR: Mount point in use! {path} is already mounted by EdenFS."
                )
                return 1
            else:
                # If we are here, MOUNT/.eden/root is a symlink, but it does not
                # point to MOUNT. This suggests `path` is a subdirectory of an
                # existing mount, though we should never reach this point
                # because _get_client_dir_for_mount_point() above should have
                # already thrown an exception. We return non-zero here just in
                # case.
                print_stderr(
                    f"ERROR: Mount point in use! "
                    f"{path} is already mounted by EdenFS as part of {root}."
                )
                return 1
        except OSError as ex:
            # - ENOENT is expected if the mount is not mounted.
            # - We'll get ENOTCONN if the directory was not properly unmounted from a
            #   previous EdenFS instance.  Remounting over this directory is okay (even
            #   though ideally we would unmount the old stale mount point to clean it
            #   up).
            # - EINVAL can happen if .eden/root isn't a symlink.  This isn't expected
            #   in most circumstances, but it does mean that the directory isn't
            #   currently an EdenFS checkout.
            err = ex.errno
            if err not in (errno.ENOENT, errno.ENOTCONN, errno.EINVAL):
                raise

        # Ask eden to mount the path
        mount_info = MountArgument(
            mountPoint=os.fsencode(path),
            edenClientPath=os.fsencode(client_dir),
            readOnly=read_only,
        )

        try:
            with self.get_thrift_client() as client:
                client.mount(mount_info)
        except EdenError as ex:
            if "already mounted" in str(ex):
                print_stderr(
                    f"ERROR: Mount point in use! {path} is already mounted by EdenFS."
                )
                return 1
            raise

        return 0

    def unmount(
        self, path: str, use_force: bool = True, unintentional_unmount: bool = False
    ) -> None:
        """Ask edenfs to unmount the specified checkout."""
        # In some cases edenfs can take a long time unmounting while it waits for
        # inodes to become unreferenced.  Ideally we should have edenfs timeout and
        # forcibly clean up the mount point in this situation.
        #
        # For now at least time out here so the CLI commands do not hang in this
        # case.
        mount_point = os.fsencode(path)
        unmount_arg = UnmountArgument(
            mountId=MountId(mountPoint=mount_point),
            useForce=use_force,
        )

        try:
            with self.get_thrift_client(timeout=UNMOUNT_TIMEOUT_SECONDS) as client:
                client.unmountV2(unmount_arg)
        finally:
            if unintentional_unmount:
                # Used by `eden unmount --debug-mark-unintentional` (tests and
                # debugging) and by doctor's unmount-and-remount repair. Leave
                # no marker, even if the call failed, so that unmount recovery
                # treats this unmount as accidental.
                self._clear_intentionally_unmounted(path)

        if not unintentional_unmount:
            # The daemon writes this marker before it starts the unmount, but
            # only best-effort, so write it again now that the unmount
            # succeeded. It stops unmount recovery from remounting the checkout.
            # The marker lives in the client directory that config.json names,
            # which a mounted-but-unregistered checkout does not have. The
            # unmount itself already succeeded, so a missing marker is a
            # warning, not a failure.
            try:
                self._set_intentionally_unmounted(path)
            except Exception as ex:
                log.warning(
                    f"unmounted {path}, but could not mark it intentionally "
                    f"unmounted: {ex}"
                )

    def _intentionally_unmounted_file(self, path: str) -> Path:
        mount_path = Path(path).resolve(strict=False)
        client_dir = self._get_client_dir_for_mount_point(mount_path)
        return client_dir / INTENTIONALLY_UNMOUNTED

    def _set_intentionally_unmounted(self, path: str) -> None:
        self._intentionally_unmounted_file(path).touch()

    def _clear_intentionally_unmounted(self, path: str) -> None:
        self._intentionally_unmounted_file(path).unlink(missing_ok=True)

    def destroy_mount(
        self, path: Union[Path, str], preserve_mount_point: bool = False
    ) -> None:
        """Delete the specified mount point from the configuration file and remove
        the mount directory, if it exists.

        This should normally be called after unmounting the mount point.
        """

        # The shutil.rmtree operation can take a lot of time and we'd like to give the user
        # a view into what it is doing. Hooking the internal recursive rmtree to display a
        # spinner
        # We're gently caressing an internal shutil function
        # pyre-ignore[16]: Module shutil has no attribute _rmtree_unsafe.
        old_rmtree_unsafe = shutil._rmtree_unsafe
        # We're gently caressing an internal shutil function
        # pyre-ignore[16]: Module shutil has no attribute _rmtree_safe_fd.
        old_rmtree_safe_fd = shutil._rmtree_safe_fd

        with Spinner("Deleting: ") as spinner:
            # pyre-ignore[2]: Parameter must be annotated.
            # pyre-fixme[3]: Return type must be annotated.
            def args_parser_for_rmtree_unsafe(args):
                try:
                    # as of Python 3.12, _rmtree_unsafe has the signature below
                    #
                    # def _rmtree_unsafe(path, onexc):
                    return str(args[0])
                except Exception:
                    return "directory and files ..."

            # pyrefly: ignore [missing-attribute]
            shutil._rmtree_unsafe = util.hook_recursive_with_spinner(
                # We're gently caressing an internal shutil function
                # pyre-ignore[16]: Module shutil has no attribute _rmtree_unsafe.
                shutil._rmtree_unsafe,
                spinner,
                args_parser_for_rmtree_unsafe,
            )

            # pyre-ignore[2]: Parameter must be annotated.
            # pyre-fixme[3]: Return type must be annotated.
            def args_parser_for_rmtree_safe_fd(args):
                try:
                    if sys.version_info >= (3, 12):
                        # as of Python 3.12, _rmtree_safe_fd has the signature below
                        #
                        # def _rmtree_safe_fd(stack, onexc):
                        #
                        # where "stack" has the structure:
                        # [
                        #  (
                        #    <built-in function rmdir>,
                        #    None,
                        #    PosixPath('/data/users/myUser/eden-dev-state/clients/fbsource-test-remove-4'),
                        #    None
                        #  )
                        # ]
                        return str(args[0][0][2])
                    else:
                        # for Python 3.11 and below, the signature is:
                        # def _rmtree_safe_fd(topfd, path, onerror):
                        return str(args[1])

                except Exception:
                    return "directory and files ..."

            # pyrefly: ignore [missing-attribute]
            shutil._rmtree_safe_fd = util.hook_recursive_with_spinner(
                # We're gently caressing an internal shutil function
                # pyre-ignore[16]: Module shutil has no attribute _rmtree_safe_fd.
                shutil._rmtree_safe_fd,
                spinner,
                args_parser_for_rmtree_safe_fd,
            )

            path = Path(path)
            _remove_client_dir(self._get_client_dir_for_mount_point(path))
            self._remove_path_from_directory_map(path)

        # Restore the original rmtree
        # pyrefly: ignore [missing-attribute]
        shutil._rmtree_unsafe = old_rmtree_unsafe
        # pyrefly: ignore [missing-attribute]
        shutil._rmtree_safe_fd = old_rmtree_safe_fd

    def can_register_running_mount(self, path: Path, client_dir: Path) -> bool:
        """True when `register_running_mount(path, client_dir)` would accept the
        pair: the client directory is one of this instance's and holds a
        checkout config, and neither the path nor the directory is registered."""
        clients_dir = self._get_clients_dir().resolve(strict=False)
        resolved = client_dir.resolve(strict=False)
        directory_map = self._get_directory_map()
        return (
            resolved.parent == clients_dir
            and (resolved / MOUNT_CONFIG).is_file()
            and Path(path) not in directory_map
            and resolved.name not in directory_map.values()
        )

    def register_running_mount(self, path: Path, client_dir: Path) -> None:
        """Add a checkout the daemon serves but config.json does not list, so
        that EdenFS remounts it after a restart and `eden rm` can remove it.

        `client_dir` is the client directory the running mount uses (its
        `.eden/client` link or the daemon's `edenClientPath`)."""
        if not self.can_register_running_mount(path, client_dir):
            raise ValueError(
                f"cannot register {path}: {client_dir} is not an unused client "
                f"directory of {self._get_clients_dir()} with a checkout config"
            )
        self._add_path_to_directory_map(
            Path(path), client_dir.resolve(strict=False).name
        )

    def _cleanup_unix_mount(self, path: Path, preserve_mount_point: bool) -> None:
        # Previous versions of EdenFS made the mount point directory read-only
        # as part of "eden clone".  Make sure it is writable now so we can clean it up.
        path.chmod(0o755)

        # Remove README_EDEN file that's known to be put there by us.
        try:
            (path / NOT_MOUNTED_README_PATH).unlink()
        except OSError as ex:
            if ex.errno != errno.ENOENT:
                raise

        if not preserve_mount_point:
            # Nuke everything under the mount path.
            shutil.rmtree(path)

    def cleanup_mount(
        self, path: Path, preserve_mount_point: bool = False, debug: bool = False
    ) -> None:
        if sys.platform != "win32":
            self._cleanup_unix_mount(path, preserve_mount_point)
        else:
            # On Windows, the mount point contains ProjectedFS placeholder and
            # files, remove all of them.

            # Embedded Python somehow cannot handle long path on Windows even if
            # it is enabled in the system. Prepending `\\?\` will let Windows
            # API to handle long path.
            windows_prefix = b"\\\\?\\"
            shutil.rmtree(windows_prefix + os.fsencode(path), ignore_errors=True)
            if not path.exists():
                return

            # Somehow, we couldn't remove some of the files, sleep a bit and retry
            time.sleep(0.5)

            errors = []

            # On Windows, we cannot remove read-only files, so we need to make
            # them writable before removing them. Let's give it a go if we are still here.
            def chmod_readonly_files(func, path, ex):
                try:
                    os.chmod(path, stat.S_IWRITE)
                    func(path)
                except Exception as e:
                    # We don't want to make things more confusing to the other by reporting errors on chmod
                    if debug:
                        print(f"  Failed to chmod {path}: {e}")

            shutil.rmtree(
                windows_prefix + os.fsencode(path), onerror=chmod_readonly_files
            )

            # See if this time (after chown) rmtree succeeded.
            if not path.exists():
                return

            def collect_errors(_f, path, ex):
                errors.append((path, ex[1]))

            shutil.rmtree(windows_prefix + os.fsencode(path), onerror=collect_errors)
            if not path.exists():
                # We were successful.
                return

            print(f"Failed to remove {path}, the following files couldn't be removed:")
            for f in errors:
                print(os.fsdecode(f[0].strip(windows_prefix)))

            # For those that failed, we might be able to do something with error codes
            # 5 ERROR_ACCESS_DENIED - Access is denied (file is a running executable).
            # 32 (ERROR_SHARING_VIOLATION) - The process cannot access the file because it is being used by another process.
            # See # https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499-
            used_by_other = any(
                filter(
                    lambda x: isinstance(x[1], OSError)
                    and (x[1].winerror == 32 or x[1].winerror == 5),
                    errors,
                )
            )

            if used_by_other:
                # Use the hammer - kill all processes
                winhr = WinFileHandlerReleaser(self)
                winhr.stop_adb_server()
                winhr.stop_buck2()
                if not winhr.try_release(
                    path
                ):  # This will return True if there's a chance it could kill the processes.
                    raise Exception("Failed to clear all resources")

                # Reset the errors because we're going to do a new pass after killing things
                errors = []
                # Try again
                shutil.rmtree(
                    windows_prefix + os.fsencode(path), onerror=collect_errors
                )
                if not path.exists():
                    # Success this time.
                    return
                print(
                    f"Failed to remove {path}, the following files couldn't be removed:"
                )
                for f in errors:
                    print(
                        f"{os.fsdecode(f[0].strip(windows_prefix))} win32 error: {f[1]}"
                    )

                print(
                    f"""
    At this point your EdenFS mount is destroyed, but EdenFS is having
    trouble cleaning up leftovers. You will need to manually remove {path}.
    """
                )

            raise Exception("Failed to delete mount.")

    def check_health(self, timeout: Optional[float] = None) -> HealthStatus:
        """
        Get the status of the edenfs daemon.

        Returns a HealthStatus object containing health information.
        """
        return util.check_health(
            self.get_thrift_client, self._config_dir, timeout=timeout
        )

    def check_privhelper_connection(self) -> bool:
        """
        Check if the PrivHelper is accessible.

        Returns True if so, False if not.
        """
        with self.get_thrift_client() as client:
            return client.checkPrivHelper().connected

    def get_log_dir(self) -> Path:
        return self._config_dir / "logs"

    def get_log_path(self) -> Path:
        return self.get_log_dir() / "edenfs.log"

    def get_checkout_config_for_path(self, path: str) -> Optional[CheckoutConfig]:
        client_link = os.path.join(path, ".eden", "client")
        try:
            client_dir = os.readlink(client_link)
        except OSError:
            return None

        checkout = EdenCheckout(self, Path(path), Path(client_dir))
        return checkout.get_config()

    def get_checkouts(self) -> List["EdenCheckout"]:
        """Return information about all configured checkouts defined in EdenFS's
        configuration file."""
        dir_map = self._get_directory_map()
        checkouts: List[EdenCheckout] = []
        clients_dir = Path(self._get_clients_dir())
        for mount_path, client_name in dir_map.items():
            checkout_data_dir = clients_dir / client_name
            checkouts.append(EdenCheckout(self, mount_path, checkout_data_dir))

        return checkouts

    def get_hg_repo(self, path: Path) -> util.HgRepo:
        return util.HgRepo(str(path))

    def _directory_map_lock(self) -> BaseFileLock:
        return FileLock(str(self._config_dir / CONFIG_JSON_LOCK))

    def _get_directory_map(self) -> Dict[Path, str]:
        """
        Parse config.json which holds a mapping of mount paths to their
        respective client directory and return contents in a dictionary.
        """
        directory_map = self._config_dir / CONFIG_JSON
        try:
            with directory_map.open() as f:
                data = json.load(f)
        except OSError as ex:
            if ex.errno != errno.ENOENT:
                raise
            data = {}
        except json.JSONDecodeError:
            raise Exception(f"invalid JSON data found in {directory_map}")

        if not isinstance(data, dict):
            raise Exception(f"invalid data found in {directory_map}")

        result: Dict[Path, str] = {}
        for k, v in data.items():
            if not isinstance(k, str) or not isinstance(v, str):
                raise Exception(f"invalid data found in {directory_map}")
            result[Path(k)] = v

        return result

    def _add_path_to_directory_map(self, path: Path, dir_name: str) -> None:
        with self._directory_map_lock():
            config_data = self._get_directory_map()
            if path in config_data:
                raise Exception(f"mount path {path} already exists.")
            config_data[path] = dir_name
            self._write_directory_map(config_data)

    def _remove_path_from_directory_map(self, path: Path) -> None:
        with self._directory_map_lock():
            config_data = self._get_directory_map()
            if path in config_data:
                del config_data[path]
                self._write_directory_map(config_data)

    def _write_directory_map(self, config_data: Dict[Path, str]) -> None:
        json_data = {str(path): name for path, name in config_data.items()}
        contents = json.dumps(json_data, indent=2, sort_keys=True) + "\n"
        write_file_atomically(self._config_dir / CONFIG_JSON, contents.encode())

    def _get_client_dir_for_mount_point(self, path: Path) -> Path:
        # The caller is responsible for making sure the path is already
        # a normalized, absolute path.
        assert path.is_absolute()

        config_data = self._get_directory_map()
        if path not in config_data:
            raise Exception(f"could not find mount path {path}")
        return self._get_clients_dir() / config_data[path]

    def _get_clients_dir(self) -> Path:
        return self._config_dir / CLIENTS_DIR

    def get_server_build_info(self) -> Dict[str, str]:
        with self.get_thrift_client(timeout=3) as client:
            return dict(client.getRegexExportedValues("^build_.*"))

    def get_uptime(self) -> datetime.timedelta:
        now = datetime.datetime.now()
        with self.get_thrift_client(timeout=3) as client:
            since_in_seconds = client.aliveSince()
        since = datetime.datetime.fromtimestamp(since_in_seconds)
        return now - since

    def do_uptime(self, pretty: bool, out: Optional[IO[bytes]] = None) -> None:
        if out is None:
            out = sys.stdout.buffer

        health_info = self.check_health()
        edenfs_pid = health_info.pid
        if edenfs_pid is None:
            running_details = f"{health_info.detail}\n"
            out.write(running_details.encode())
            return

        uptime = self.get_uptime()  # Check if uptime is negative?
        days = uptime.days
        hours, remainder = divmod(uptime.seconds, 3600)
        minutes, seconds = divmod(remainder, 60)

        if pretty:
            if not health_info.is_healthy():
                not_healthy = f"edenfs (pid: {edenfs_pid}) is not healthy\n"
                out.write(not_healthy.encode())

            pretty_uptime = f"edenfs uptime (pid {edenfs_pid}): {datetime.timedelta(days=days, hours=hours, minutes=minutes, seconds=seconds)}\n"
            out.write(pretty_uptime.encode())

        else:
            out.write(b"%dd:%02dh:%02dm:%02ds\n" % (days, hours, minutes, seconds))

    def read_local_config(self) -> configutil.EdenConfigParser:
        return self.read_configs([self.user_config_path])

    def write_local_config(self, config: configutil.EdenConfigParser) -> None:
        """
        Writes TOML config to the local config path.
        NOTE: this method will write an empty file if the config is empty
        """
        write_file_atomically(
            self.user_config_path, toml.dumps(config.to_raw_dict()).encode()
        )


class EdenCheckout:
    """Information about a particular EdenFS checkout."""

    def __init__(self, instance: EdenInstance, path: Path, state_dir: Path) -> None:
        self.instance = instance
        self.path = path

        self.state_dir = state_dir
        self._config: Optional[CheckoutConfig] = None
        self.name: str = path.name

    def __repr__(self) -> str:
        return f"EdenCheckout({self.instance!r}, {self.path!r}, {self.state_dir!r})"

    def get_relative_path(self, path: Path, already_resolved: bool = False) -> Path:
        """Compute the relative path to a given location inside an EdenFS checkout.

        If the checkout is currently mounted this function is able to correctly resolve
        paths that refer into the checkout via alternative bind mount locations.
        e.g.  if the checkout is located at "/home/user/foo/eden_checkout" but
        "/home/user" is also bind-mounted to "/data/user" this will still be able to
        correctly resolve an input path of "/data/user/foo/eden_checkout/test"
        """
        if not already_resolved:
            path = path.resolve(strict=False)

        # First try using path.relative_to()
        # This should work in the common case
        try:
            return path.relative_to(self.path)
        except ValueError:
            pass

        # path.relative_to() may fail if the checkout is bind-mounted to an alternate
        # location, and the input path points into it using the bind mount location.
        # In this case search upwards from the input path looking for the checkout root.
        try:
            path_stat = path.lstat()
        except OSError as ex:
            raise Exception(
                f"unable to stat {path} to find relative location inside "
                f"checkout {self.path}: {ex}"
            )

        try:
            root_stat = self.path.lstat()
        except OSError as ex:
            raise Exception(f"unable to stat checkout at {self.path}: {ex}")

        if (path_stat.st_dev, path_stat.st_ino) == (root_stat.st_dev, root_stat.st_ino):
            # This is the checkout root
            return Path()

        curdir = path.parent
        path_parts = [path.name]
        while True:
            stat = curdir.lstat()
            if (stat.st_dev, stat.st_ino) == (root_stat.st_dev, root_stat.st_ino):
                path_parts.reverse()
                return Path(*path_parts)

            if curdir.parent == curdir:
                raise Exception(
                    f"unable to determine relative location of {path} "
                    f"inside {self.path}"
                )

            path_parts.append(curdir.name)
            curdir = curdir.parent

    # only for use in unit tests, in production the config should always be read
    # from disk
    def set_config(self, config: CheckoutConfig) -> None:
        self._config = config

    def get_config(self) -> CheckoutConfig:
        config = self._config
        if config is None:
            config = self._read_config()
            self._config = config
        return config

    def save_config(self, checkout_config: CheckoutConfig) -> None:
        # Store information about the mount in the config.toml file.

        # This is a little gross, but only needs to live long enough
        # to swing through migrating folks away from the legacy
        # configuration.

        redirections = {k: str(v) for k, v in checkout_config.redirections.items()}
        config_data = {
            "repository": {
                # TODO: replace is needed to workaround a bug in toml
                "path": str(checkout_config.backing_repo).replace("\\", "/"),
                "type": checkout_config.scm_type,
                "guid": checkout_config.guid,
                "protocol": checkout_config.mount_protocol,
                "case-sensitive": checkout_config.case_sensitive,
                "require-utf8-path": checkout_config.require_utf8_path,
                "use-write-back-cache": checkout_config.use_write_back_cache,
                "inode-catalog-type": checkout_config.inode_catalog_type,
                "off-mount-repo-dir": checkout_config.off_mount_repo_dir,
            },
            "redirections": redirections,
            "profiles": {
                "active": checkout_config.active_prefetch_profiles,
            },
            "recas": {
                "use-case": checkout_config.re_use_case,
            },
        }

        util.write_file_atomically(
            self._config_path(), toml.dumps(config_data).encode()
        )

        # Update our local config cache
        self._config = checkout_config

    @functools.cached_property
    def hg_dot_path(self) -> Path:
        from . import hg_util

        return self.path / hg_util.sniff_dot_dir(self.path)

    def _config_path(self) -> Path:
        return self.state_dir / MOUNT_CONFIG

    def _read_config(self) -> CheckoutConfig:
        """Returns CheckoutConfig or raises an Exception if the config.toml
        under self.state_dir is not properly formatted or does not exist.
        """
        config_path: Path = self._config_path()
        try:
            config = load_toml_config(config_path)
        except FileNotFoundError as e:
            raise CheckoutConfigCorruptedError(
                f"{config_path} does not exist. {e}"
            ) from e
        except FileError as e:
            raise CheckoutConfigCorruptedError(f"{e}") from e
        repo_field = config.get("repository")
        if isinstance(repo_field, dict):
            repository: typing.Mapping[str, str] = repo_field
        else:
            raise CheckoutConfigCorruptedError(f"{config_path} is missing [repository]")

        def get_field(key: str) -> str:
            value = repository.get(key)
            if isinstance(value, str):
                return value
            raise CheckoutConfigCorruptedError(
                f"{config_path} is missing {key} in [repository]"
            )

        scm_type = get_field("type")
        if scm_type not in SUPPORTED_REPOS:
            raise CheckoutConfigCorruptedError(
                f'repository "{config_path}" has unsupported type "{scm_type}"'
            )

        mount_protocol = repository.get("protocol")
        if not isinstance(mount_protocol, str):
            mount_protocol = util.get_platform_default_mount_protocol()
        if mount_protocol not in SUPPORTED_MOUNT_PROTOCOLS:
            raise CheckoutConfigCorruptedError(
                f'repository "{config_path}" has unsupported mount protocol '
                f'"{mount_protocol}"'
            )

        guid = repository.get("guid")
        if not isinstance(guid, str):
            guid = str(uuid.uuid4())

        case_sensitive = repository.get("case-sensitive")
        if not isinstance(case_sensitive, bool):
            # For existing repositories, keep it case sensitive
            case_sensitive = sys.platform != "win32"

        require_utf8_path = repository.get("require-utf8-path")
        if not isinstance(require_utf8_path, bool):
            # Existing repositories may have non-utf8 files, thus allow them.
            require_utf8_path = True

        redirections = {}
        redirections_dict = config.get("redirections")

        if redirections_dict is not None:
            from eden.fs.cli.redirect import RedirectionType  # noqa: F811

            if not isinstance(redirections_dict, dict):
                raise CheckoutConfigCorruptedError(
                    f"{config_path} has an invalid [redirections] section"
                )
            for key, value in redirections_dict.items():
                if not isinstance(value, str):
                    raise CheckoutConfigCorruptedError(
                        f"{config_path} has invalid value in "
                        f"[redirections] for {key}: {value} "
                        "(string expected)"
                    )
                try:
                    redirections[key] = RedirectionType.from_arg_str(value)
                except ValueError as exc:
                    raise CheckoutConfigCorruptedError(
                        f"{config_path} has invalid value in "
                        f"[redirections] for {key}: {value} "
                        f"{str(exc)}"
                    )

        prefetch_profiles = []
        prefetch_profiles_list = config.get("profiles")

        if prefetch_profiles_list is not None:
            prefetch_profiles_list = prefetch_profiles_list.get("active")
            if prefetch_profiles_list is not None:
                if not isinstance(prefetch_profiles_list, list):
                    raise CheckoutConfigCorruptedError(
                        f"{config_path} has an invalid [profiles] section"
                    )
                for profile in prefetch_profiles_list:
                    if not isinstance(profile, str):
                        raise CheckoutConfigCorruptedError(
                            f"{config_path} has invalid value in "
                            f"[profiles] {profile} (string expected)"
                        )

                    prefetch_profiles.append(profile)

        use_write_back_cache = repository.get("use-write-back-cache")
        if not isinstance(use_write_back_cache, bool):
            use_write_back_cache = False

        re_use_case = "buck2-default"
        recas = config.get("recas")
        if recas is not None:
            if recas.get("use-case") is not None:
                re_use_case = str(recas.get("use-case"))

        off_mount_repo_dir = repository.get("off-mount-repo-dir")
        if not isinstance(off_mount_repo_dir, bool):
            off_mount_repo_dir = False

        # A type this CLI does not know is kept as written rather than
        # rejected: the daemon refuses to mount such a checkout, and the user
        # still needs `eden rm`, `eden list` and `eden doctor` to work on it.
        # `eden clone` validates the type for new checkouts.
        inode_catalog_type = repository.get("inode-catalog-type")
        if inode_catalog_type is not None:
            if not isinstance(inode_catalog_type, str):
                raise CheckoutConfigCorruptedError(
                    f'repository "{config_path}" has a non-string inode catalog '
                    f'(overlay) type "{inode_catalog_type}".'
                )
            inode_catalog_type = inode_catalog_type.lower()
            if sys.platform == "win32" and inode_catalog_type in [
                "legacy",
                "legacyephemeral",
            ]:
                raise CheckoutConfigCorruptedError(
                    "Legacy inode catalog (overlay) type not supported on Windows. "
                    "Use InMemory on Windows."
                )
            elif sys.platform != "win32" and inode_catalog_type == "inmemory":
                raise CheckoutConfigCorruptedError(
                    "InMemory inode catalog (overlay) type is only supported on Windows. "
                    "Use Legacy on Linux and MacOS."
                )

        return CheckoutConfig(
            backing_repo=Path(get_field("path")),
            scm_type=scm_type,
            guid=guid,
            case_sensitive=case_sensitive,
            require_utf8_path=require_utf8_path,
            mount_protocol=mount_protocol,
            redirections=redirections,
            redirection_targets={},  # There is no need to read redirection targets in Python CLI for now.
            default_revision=(
                repository.get("default-revision") or DEFAULT_REVISION[scm_type]
            ),
            active_prefetch_profiles=prefetch_profiles,
            use_write_back_cache=use_write_back_cache,
            re_use_case=re_use_case,
            inode_catalog_type=inode_catalog_type,
            off_mount_repo_dir=off_mount_repo_dir,
        )

    def parse_snapshot_component(self, buf: bytes) -> Tuple[str, Optional[bytes]]:
        return parse_snapshot_component(buf, self.get_config().scm_type)

    def get_snapshot(self) -> SnapshotState:
        return get_snapshot(self.state_dir / SNAPSHOT, self.get_config().scm_type)

    def save_snapshot(self, commit_id: bytes) -> None:
        """Write a new parent commit ID into the SNAPSHOT file."""
        snapshot_path = self.state_dir / SNAPSHOT
        write_file_atomically(
            snapshot_path,
            SNAPSHOT_MAGIC_2 + struct.pack(">L", len(commit_id)) + commit_id,
        )

    def get_backing_repo(self) -> util.HgRepo:
        repo_path = self.get_config().backing_repo
        return self.instance.get_hg_repo(repo_path)

    def get_backing_repo_path(self) -> Path:
        return self.get_config().backing_repo

    def migrate_mount_protocol(self, new_mount_protocol: str) -> None:
        """
        Migrate this checkout to the new_mount_protocol. This will only take
        effect if EdenFS is restarted. It is recommended to only run this while
        EdenFS is stopped.
        """

        old_config = self.get_config()

        new_config = old_config._replace(mount_protocol=new_mount_protocol)

        self.save_config(new_config)

    def wait_until_mounted(self, timeout_sec: int = 5) -> bool:
        """
        Wait until the checkout is mounted.
        Returns True if the checkout is mounted
        False if the timeout is reached.
        """
        start_time = time.time()
        while time.time() - start_time < timeout_sec:
            mounts = self.instance.get_mounts()
            for path, mount_info in mounts.items():
                if path == self.path and mount_info.state == MountState.RUNNING:
                    return True
            time.sleep(0.5)
        return False


def get_snapshot(snapshot_path: Path, scm_type: str) -> SnapshotState:
    """Return the hex version of the parent hash in the SNAPSHOT file."""
    if not snapshot_path.exists():
        raise RuntimeError(f"Missing SNAPSHOT file {snapshot_path}")
    with snapshot_path.open("rb") as f:
        header = f.read(8)
        if header == SNAPSHOT_MAGIC_1:
            decoded_parent = binascii.hexlify(f.read(20)).decode()
            return SnapshotState(
                working_copy_parent=decoded_parent,
                last_checkout_hash=decoded_parent,
            )
        elif header == SNAPSHOT_MAGIC_2:
            (bodyLength,) = struct.unpack(">L", f.read(4))
            parent = f.read(bodyLength)
            if len(parent) != bodyLength:
                raise RuntimeError("SNAPSHOT file too short")
            decoded_parent, filter_bytes = parse_snapshot_component(parent, scm_type)
            return SnapshotState(
                working_copy_parent=decoded_parent,
                last_checkout_hash=decoded_parent,
                parent_filter_id=filter_bytes,
                last_filter_id=filter_bytes,
            )
        elif header == SNAPSHOT_MAGIC_3:
            (pid,) = struct.unpack(">L", f.read(4))

            (fromLength,) = struct.unpack(">L", f.read(4))
            fromParent = f.read(fromLength)
            if len(fromParent) != fromLength:
                raise RuntimeError("SNAPSHOT file too short")
            (toLength,) = struct.unpack(">L", f.read(4))
            toParent = f.read(toLength)
            if len(toParent) != toLength:
                raise RuntimeError("SNAPSHOT file too short")

            decoded_to, _ = parse_snapshot_component(toParent, scm_type)
            decoded_from, _ = parse_snapshot_component(fromParent, scm_type)

            raise InProgressCheckoutError(decoded_from, decoded_to, pid)
        elif header == SNAPSHOT_MAGIC_4:
            (working_copy_parent_length,) = struct.unpack(">L", f.read(4))
            working_copy_parent = f.read(working_copy_parent_length)
            if len(working_copy_parent) != working_copy_parent_length:
                raise RuntimeError("SNAPSHOT file too short")
            (checked_out_length,) = struct.unpack(">L", f.read(4))
            checked_out_revision = f.read(checked_out_length)
            if len(checked_out_revision) != checked_out_length:
                raise RuntimeError("SNAPSHOT file too short")

            working_copy_parent, parent_filter_bytes = parse_snapshot_component(
                working_copy_parent, scm_type
            )
            (
                checked_out_revision,
                checked_out_filter_bytes,
            ) = parse_snapshot_component(checked_out_revision, scm_type)
            return SnapshotState(
                working_copy_parent=working_copy_parent,
                last_checkout_hash=checked_out_revision,
                parent_filter_id=parent_filter_bytes,
                last_filter_id=checked_out_filter_bytes,
            )
        else:
            raise RuntimeError("SNAPSHOT file has invalid header")


def parse_snapshot_component(buf: bytes, scm_type: str) -> Tuple[str, Optional[bytes]]:
    """Parse a component from the snapshot file.

    Returns a tuple containing the parsed hash and a filter id (if
    applicable). For unfiltered repos, None is always returned for the
    component's filter id.
    """
    decoded_hash = ""
    filter_bytes = None
    if scm_type == "filteredhg":
        hash_len, varint_len = util.decode_varint(buf)
        filter_offset = varint_len + hash_len
        encoded_hash = buf[varint_len:filter_offset]
        filter_bytes = buf[filter_offset:]
        decoded_hash = encoded_hash.decode()
    else:
        decoded_hash = buf.decode()

    return decoded_hash, filter_bytes


_FUSE_USE_IO_URING = "fuse.use-io-uring"
_FUSE_IO_URING_KERNEL_RELEASE_REGEX = "fuse.io-uring-kernel-release-regex"
_FUSE_RESTART_ON_TRANSPORT_MISMATCH = "fuse.restart-on-transport-mismatch"
_DEFAULT_FUSE_IO_URING_KERNEL_RELEASE_REGEX = r"^6\.13\."

FUSE_TRANSPORT_DEVFUSE = "devfuse"
FUSE_TRANSPORT_IO_URING = "io_uring"

_NFS_USE_UDS = "nfs.use-uds"
_NFS_RESTART_ON_TRANSPORT_MISMATCH = "nfs.restart-on-transport-mismatch"

NFS_TRANSPORT_TCP = "tcp"
NFS_TRANSPORT_UNIX = "unix"


class TransportMismatch(typing.NamedTuple):
    """A mount whose channel transport differs from the configured one."""

    mount: Path
    active_transport: str
    desired_transport: str
    # "fuse" or "nfs"
    channel: str = "fuse"


FuseTransportMismatch = TransportMismatch


def is_fuse_transport_mismatch_restart_enabled(
    instance: AbstractEdenInstance,
) -> bool:
    return sys.platform == "linux" and instance.get_config_bool(
        _FUSE_RESTART_ON_TRANSPORT_MISMATCH, default=False
    )


def get_desired_fuse_transport(instance: AbstractEdenInstance) -> Optional[str]:
    if sys.platform != "linux":
        return None

    if not instance.get_config_bool(_FUSE_USE_IO_URING, default=False):
        return FUSE_TRANSPORT_DEVFUSE

    kernel_release_regex = instance.get_config_value(
        _FUSE_IO_URING_KERNEL_RELEASE_REGEX,
        default=_DEFAULT_FUSE_IO_URING_KERNEL_RELEASE_REGEX,
    )
    if not kernel_release_regex:
        return FUSE_TRANSPORT_DEVFUSE

    try:
        if re.search(kernel_release_regex, os.uname().release) is not None:
            return FUSE_TRANSPORT_IO_URING
    except re.error as ex:
        log.warning(
            "Invalid FUSE io_uring kernel release regex %r: %s",
            kernel_release_regex,
            ex,
        )

    return FUSE_TRANSPORT_DEVFUSE


def get_fuse_transport_mismatches(
    instance: AbstractEdenInstance,
) -> List[FuseTransportMismatch]:
    if sys.platform != "linux":
        return []

    desired_transport = get_desired_fuse_transport(instance)
    if desired_transport is None:
        return []

    mismatches: List[FuseTransportMismatch] = []
    for mount_info in instance.get_mounts().values():
        active_transport = mount_info.fuse_transport
        if active_transport is None:
            continue
        if active_transport != desired_transport:
            mismatches.append(
                FuseTransportMismatch(
                    mount=mount_info.path,
                    active_transport=active_transport,
                    desired_transport=desired_transport,
                )
            )
    return mismatches


def is_nfs_transport_mismatch_restart_enabled(
    instance: AbstractEdenInstance,
) -> bool:
    return sys.platform != "win32" and instance.get_config_bool(
        _NFS_RESTART_ON_TRANSPORT_MISMATCH, default=False
    )


def get_desired_nfs_transport(instance: AbstractEdenInstance) -> str:
    if instance.get_config_bool(_NFS_USE_UDS, default=False):
        return NFS_TRANSPORT_UNIX
    return NFS_TRANSPORT_TCP


def get_nfs_transport_mismatches(
    instance: AbstractEdenInstance,
) -> List[TransportMismatch]:
    """NFS mounts whose transport differs from nfs.use-uds.

    A running daemon keeps the transport its mountd was created with, so
    after the setting changes every NFS mount mismatches until a full
    restart.
    """
    desired_transport = get_desired_nfs_transport(instance)
    return [
        TransportMismatch(
            mount=mount_info.path,
            active_transport=mount_info.nfs_transport,
            desired_transport=desired_transport,
            channel="nfs",
        )
        for mount_info in instance.get_mounts().values()
        if mount_info.nfs_transport is not None
        and mount_info.nfs_transport != desired_transport
    ]


def get_transport_mismatches(
    instance: AbstractEdenInstance,
) -> List[TransportMismatch]:
    """Mismatches of every channel whose restart-on-mismatch flag is on."""
    mismatches: List[TransportMismatch] = []
    if is_fuse_transport_mismatch_restart_enabled(instance):
        mismatches.extend(get_fuse_transport_mismatches(instance))
    if is_nfs_transport_mismatch_restart_enabled(instance):
        mismatches.extend(get_nfs_transport_mismatches(instance))
    return mismatches


def _do_manual_migration(
    instance: EdenInstance,
    migrate_to: str,
    get_migration_success_message: Callable[[str], str],
) -> None:
    for checkout in instance.get_checkouts():
        checkout.migrate_mount_protocol(migrate_to)

    print(get_migration_success_message(migrate_to))


def detect_checkout_path_problem(
    path: Union[str, Path],
    instance: EdenInstance,
) -> Tuple[Optional[CheckoutPathProblemType], Optional[EdenCheckout]]:
    """
    Get a tuple containing (problem_type, checkout, rel_path) for any checkout that the provided
    path is nested inside of, or for any checkout whose
    backing_repo contains the provided path and the relative path of that
    path within the backing_repo, along with the problem type.

    A tuple of (None, None, None) is returned if the specified path is not nested inside
    any existing checkouts.
    """
    if isinstance(path, str):
        path = Path(path)

    path = path.resolve(strict=False)
    try:
        # However, we prefer to get the list from the current eden process (if one's running)
        instance.get_running_version()
        checkout_list = instance.get_mounts().items()
    except EdenNotRunningError:
        # If EdenFS isn't running, we should fail
        return None, None

    # Checkout list must be sorted so that parent paths are checked first
    for checkout_path_str, mount_info in sorted(checkout_list):
        # symlinks could have been added since the mount was added, but
        # we will not worry about this case
        checkout_path = Path(checkout_path_str)
        if path != checkout_path and is_child_path(checkout_path, path):
            checkout_state_dir = mount_info.data_dir
            checkout = EdenCheckout(instance, checkout_path, checkout_state_dir)
            return CheckoutPathProblemType.NESTED_CHECKOUT, checkout

        # check if path is inside backing folder of the current checkout
        backing_repo = mount_info.backing_repo
        if backing_repo is not None and is_child_path(backing_repo, path):
            checkout_state_dir = mount_info.data_dir
            checkout = EdenCheckout(instance, checkout_path_str, checkout_state_dir)
            return CheckoutPathProblemType.INSIDE_BACKING_REPO, checkout

    return None, None


def is_child_path(parent_path: Path, child_path: Path) -> bool:
    """Returns true if the parent path is a prefix of the child path"""
    try:
        rel_path = child_path.relative_to(parent_path)
        return rel_path != Path("") and rel_path != Path(".")
    except ValueError:
        return False


def find_eden(
    path: Union[str, Path],
    etc_eden_dir: Optional[str] = None,
    home_dir: Optional[str] = None,
    state_dir: Optional[str] = None,
) -> Tuple[EdenInstance, Optional[EdenCheckout], Optional[Path]]:
    """Look up the EdenInstance and EdenCheckout for a path.

    If the input path points into an EdenFS checkout, this returns a tuple of
    (EdenInstance, EdenCheckout, rel_path), where EdenInstance contains information for
    the edenfs instance serving this checkout, EdenCheckout contains information about
    the checkout, and rel_path contains the relative location of the input path inside
    the checkout.  The checkout does not need to be currently mounted for this to work.

    If the input path does not point inside a known EdenFS checkout, this returns
    (EdenInstance, None, None)
    """
    if isinstance(path, str):
        path = Path(path)

    path = path.resolve(strict=False)

    # First check to see if this looks like a mounted checkout
    eden_state_dir = None
    checkout_root = None
    checkout_state_dir = None
    try:
        if sys.platform != "win32":
            eden_socket_path = os.readlink(path.joinpath(path, ".eden", "socket"))
            eden_state_dir = os.path.dirname(eden_socket_path)

            checkout_root = Path(os.readlink(path.joinpath(".eden", "root")))
            checkout_state_dir = Path(os.readlink(path.joinpath(".eden", "client")))
        else:
            # On Windows, walk the path backwards until both parent and dir
            # point to "C:\"
            curdir = path
            while curdir != curdir.parent:
                try:
                    tomlconfig = toml.load(curdir / ".eden" / "config")
                except FileNotFoundError:
                    curdir = curdir.parent
                    continue

                eden_socket_path = tomlconfig["Config"]["socket"]
                eden_state_dir = os.path.dirname(eden_socket_path)
                checkout_root = Path(tomlconfig["Config"]["root"])
                checkout_state_dir = Path(tomlconfig["Config"]["client"])
                break

    except OSError:
        # We will get an OSError if any of these symlinks do not exist
        # Fall through and we will handle this below.
        pass

    if eden_state_dir is None:
        # Use the state directory argument supplied by the caller.
        # If this is None the EdenInstance constructor will pick the correct location.
        eden_state_dir = state_dir
    elif state_dir is not None:
        # We found a state directory from the checkout and the user also specified an
        # explicit state directory.  Make sure they match.
        _check_same_eden_directory(Path(eden_state_dir), Path(state_dir))

    instance = EdenInstance(
        eden_state_dir, etc_eden_dir=etc_eden_dir, home_dir=home_dir
    )
    checkout: Optional[EdenCheckout] = None
    rel_path: Optional[Path] = None
    if checkout_root is None:
        all_checkouts = instance._get_directory_map()
        for checkout_path_str, checkout_name in all_checkouts.items():
            checkout_path = Path(checkout_path_str)
            try:
                rel_path = path.relative_to(checkout_path)
            except ValueError:
                continue

            checkout_state_dir = instance.state_dir.joinpath(CLIENTS_DIR, checkout_name)
            checkout = EdenCheckout(instance, checkout_path, checkout_state_dir)
            break
        else:
            # This path does not appear to be inside a known checkout
            checkout = None
            rel_path = None
    elif checkout_state_dir is None:
        all_checkouts = instance._get_directory_map()
        checkout_name_value = all_checkouts.get(checkout_root)
        if checkout_name_value is None:
            raise Exception(f"unknown checkout {checkout_root}")
        checkout_state_dir = instance.state_dir.joinpath(
            CLIENTS_DIR, checkout_name_value
        )
        checkout = EdenCheckout(instance, checkout_root, checkout_state_dir)
        rel_path = checkout.get_relative_path(path, already_resolved=True)
    else:
        checkout = EdenCheckout(instance, checkout_root, checkout_state_dir)
        rel_path = checkout.get_relative_path(path, already_resolved=True)

    return (instance, checkout, rel_path)


def eden_instance_from_cmdline(cmdline: List[bytes]) -> EdenInstance:
    try:
        eden_dir_idx = cmdline.index(b"--edenDir") + 1
        eden_dir = Path(cmdline[eden_dir_idx].decode("utf-8"))
    except ValueError:
        eden_dir = None

    try:
        etc_eden_dir_idx = cmdline.index(b"--etcEdenDir") + 1
        etc_eden_dir = Path(cmdline[etc_eden_dir_idx].decode("utf-8"))
    except ValueError:
        etc_eden_dir = None
    try:
        config_path_idx = cmdline.index(b"--configPath") + 1
        config_path = Path(cmdline[config_path_idx].decode("utf-8")).parent
    except ValueError:
        config_path = None

    return EdenInstance(eden_dir, etc_eden_dir, config_path)


def _check_same_eden_directory(found_path: Path, path_arg: Path) -> None:
    s1 = found_path.lstat()
    s2 = path_arg.lstat()
    if (s1.st_dev, s1.st_ino) != (s2.st_dev, s2.st_ino):
        raise Exception(
            f"the specified directory is managed by the edenfs instance at "
            f"{found_path}, which is different from the explicitly requested "
            f"instance at {path_arg}"
        )


def _verify_mount_point(mount_point: str) -> None:
    if os.path.isdir(mount_point):
        return
    parent_dir = os.path.dirname(mount_point)
    if os.path.isdir(parent_dir):
        os.mkdir(mount_point)
    else:
        raise Exception(
            (
                "%s must be a directory in order to mount a client at %s. "
                + "If this is the correct location, run `mkdir -p %s` to create "
                + "the directory."
            )
            % (parent_dir, mount_point, parent_dir)
        )


TomlConfigDict = Mapping[str, Mapping[str, Any]]


def get_line_by_number(contents: str, line_num: int) -> Optional[str]:
    lines = contents.splitlines()
    if len(lines) < line_num or line_num < 1:
        return None
    return lines[line_num - 1]


def get_line_number_from_exception_message(s: str) -> int:
    match = re.search(r"line (\d+)", s)
    if match:
        return int(match.group(1))
    else:
        return -1


def load_toml_config(path: Path) -> TomlConfigDict:
    data: str = ""
    hint: str = ""
    try:
        with open(path, "r") as file:
            data = file.read()
            return typing.cast(TomlConfigDict, toml.loads(data))
    except FileNotFoundError:
        raise
    except ValueError as e:
        print(f"Value error: {e}")
        if e.args[0].startswith(
            "Reserved escape sequence used"
        ):  # OK to hardcode this text; it's hardcoded in toml lib too
            hint = "\nHint: Check that you don't have single backslashes.\n"
        line_num = get_line_number_from_exception_message(e.args[0])
        if line_num != -1:
            line = get_line_by_number(data, line_num)
            if line is not None:
                hint += f"Detected here (line {line_num}):\n\n{line}\n"

        raise FileError(f"toml config file {str(path)} not valid: {str(e)}{hint}")
    except Exception as e:
        raise FileError(
            f"toml config file {str(path)} is either missing or corrupted: {str(e)}"
        )


def get_repo_info(
    instance: EdenInstance,
    repo_arg: str,
    rev: Optional[str],
    nfs: bool,
    case_sensitive: bool,
    overlay_type: Optional[str],
    backing_store_type: Optional[str] = None,
    re_use_case: Optional[str] = None,
    off_mount_repo_dir: bool = False,
) -> Tuple[util.Repo, CheckoutConfig]:
    # Check to see if repo_arg points to an existing EdenFS mount
    checkout_config = instance.get_checkout_config_for_path(repo_arg)
    if checkout_config is not None:
        if backing_store_type is not None:
            # If the user specified a backing store type, make sure it takes
            # priority over the existing checkout config.
            if backing_store_type != checkout_config.scm_type:
                checkout_config = checkout_config._replace(scm_type=backing_store_type)

        repo = util.get_repo(str(checkout_config.backing_repo), backing_store_type)
        if repo is None:
            raise util.RepoError(
                "EdenFS mount is configured to use repository "
                f"{checkout_config.backing_repo} but unable to find a "
                "repository at that location"
            )
        return repo, checkout_config

    # Confirm that repo_arg looks like an existing repository path.
    repo = util.get_repo(repo_arg, backing_store_type)
    if repo is None:
        raise util.RepoError(f"{repo_arg!r} does not look like a valid repository")
    checkout_config = create_checkout_config(
        repo,
        instance,
        nfs,
        case_sensitive,
        overlay_type,
        backing_store_type=backing_store_type,
        re_use_case=re_use_case,
        off_mount_repo_dir=off_mount_repo_dir,
    )

    return repo, checkout_config


def create_checkout_config(
    repo: util.Repo,
    instance: EdenInstance,
    nfs: bool,
    case_sensitive: bool,
    overlay_type: Optional[str],
    backing_store_type: Optional[str] = None,
    re_use_case: Optional[str] = None,
    off_mount_repo_dir: bool = False,
) -> CheckoutConfig:
    mount_protocol = util.get_protocol(nfs)

    if overlay_type is None:
        # Not specified - read from EdenConfig, fallback to default.
        overlay_type = instance.get_config_value(
            "overlay.inode-catalog-type",
            "inmemory" if sys.platform == "win32" else "legacy",
        )
        if overlay_type not in SUPPORTED_INODE_CATALOG_TYPES:
            raise Exception(
                f"Eden config has unsupported overlay (inode catalog) type "
                f'"{overlay_type}". Supported overlay (inode catalog) types are: '
                f"{', '.join(sorted(SUPPORTED_INODE_CATALOG_TYPES))}."
            )
    overlay_type = overlay_type.lower()
    if sys.platform == "win32" and overlay_type in [
        "legacy",
        "legacyephemeral",
    ]:
        raise Exception(
            "Legacy overlay (inode catalog) type not supported on Windows. "
            "Use InMemory on Windows."
        )
    elif sys.platform != "win32" and overlay_type == "inmemory":
        raise Exception(
            "InMemory overlay (inode catalog) type is only supported on Windows. "
            "Use Legacy on Linux and MacOS."
        )

    # This is a valid repository path.
    # Prepare a CheckoutConfig object for it.
    repo_config = CheckoutConfig(
        backing_repo=Path(repo.source),
        scm_type=repo.type,
        guid=str(uuid.uuid4()),
        mount_protocol=mount_protocol,
        case_sensitive=case_sensitive,
        require_utf8_path=True,
        default_revision=DEFAULT_REVISION[repo.type],
        redirections={},
        redirection_targets={},
        active_prefetch_profiles=[],
        use_write_back_cache=False,
        re_use_case=re_use_case or "buck2-default",
        inode_catalog_type=overlay_type,
        off_mount_repo_dir=off_mount_repo_dir,
    )

    return repo_config
