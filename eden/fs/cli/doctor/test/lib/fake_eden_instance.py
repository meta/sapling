#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict


import binascii
import os
import shutil
import stat
import sys
import typing
import uuid
from contextlib import contextmanager
from pathlib import Path
from typing import Dict, Generator, Iterable, List, NamedTuple, Optional, Tuple, Union

import eden.dirstate
from eden.fs.cli import mtab, telemetry, version as version_mod
from eden.fs.cli.config import (
    AbstractEdenInstance,
    CheckoutConfig,
    configutil,
    EdenCheckout,
    EdenInstance,
    HealthStatus,
    ListMountInfo,
)
from fb303_core.thrift_types import fb303_status

from .fake_client import FakeClient
from .fake_constants import FAKE_UID
from .fake_hg_repo import FakeHgRepo
from .fake_mount_table import FakeMountTable


class FakeCheckout(NamedTuple):
    state_dir: Path
    config: CheckoutConfig
    snapshot: str


class FakeEdenInstance(AbstractEdenInstance):
    """Mock EdenFS instance for testing EdenFS doctor and CLI functionality.

    This test utility simulates an EdenFS instance with configurable mounts,
    checkouts, and server state. Use it to test EdenFS doctor checks, mount
    operations, and configuration without requiring a running EdenFS daemon.

    Key features:
    - Create test mounts with create_test_mount() to simulate active/inactive checkouts
    - Configure server status (ALIVE, STARTING, STOPPING) and build info
    - Manage fake mount tables, backing repos, and HG repositories
    - Access test state through get_checkouts(), get_mounts(), and health checks

    Example:
        instance = FakeEdenInstance(tmp_dir)
        checkout = instance.create_test_mount("my_mount", snapshot="abc123")
        # Use instance for testing doctor checks or CLI operations
    """

    default_commit_hash: str = "1" * 40
    _build_info: Dict[str, str] = {}

    def __init__(
        self,
        tmp_dir: str,
        status: fb303_status = fb303_status.ALIVE,
        build_info: Optional[Dict[str, str]] = None,
        config: Optional[Dict[str, str]] = None,
    ) -> None:
        self._tmp_dir = tmp_dir
        self._status = status
        self._build_info = build_info if build_info else {}
        # pyre-fixme[4]: Attribute must be annotated.
        self._config = config if config else {}

        # pyre-fixme[4]: Attribute must be annotated.
        self._eden_dir = Path(self._tmp_dir) / "eden"
        self._eden_dir.mkdir()
        # pyre-fixme[4]: Attribute must be annotated.
        self.clients_path = self._eden_dir / "clients"
        self.clients_path.mkdir()
        # pyre-fixme[4]: Attribute must be annotated.
        self.default_backing_repo = (
            Path(self._tmp_dir) / ".eden-backing-repos" / "main_repo"
        )
        (self.default_backing_repo / ".hg").mkdir(parents=True)

        # A map from mount path --> FakeCheckout
        self._checkouts_by_path: Dict[str, FakeCheckout] = {}
        self._hg_repo_by_path: Dict[Path, FakeHgRepo] = {}
        self.mount_table = FakeMountTable()
        self._next_dev_id = 10
        self.mount_calls: List[Tuple[str, bool]] = []
        self.unmount_calls: List[Tuple[str, bool, bool]] = []
        self.logged_samples: List[Tuple[str, Dict[str, telemetry.TelemetryTypes]]] = []

        self._fake_client = FakeClient(self._eden_dir, self.mount_table)

    @property
    def state_dir(self) -> Path:
        return self._eden_dir

    def create_test_mount(
        self,
        path: str,
        snapshot: Optional[str] = None,
        client_name: Optional[str] = None,
        scm_type: str = "hg",
        active: bool = True,
        setup_path: bool = True,
        dirstate_parent: Union[str, Tuple[str, str], None] = None,
        backing_repo: Optional[Path] = None,
        mount_protocol: Optional[str] = None,
    ) -> EdenCheckout:
        """
        Define a configured mount.

        If active is True and status was set to ALIVE when creating the FakeClient
        then the mount will appear as a normal active mount.  It will be reported in the
        thrift results and the mount table, and the mount directory will be populated
        with a .hg/ or .git/ subdirectory.

        The setup_path argument can be set to False to prevent creating the fake mount
        directory on disk.

        Returns the absolute path to the mount directory.
        """
        full_path = os.path.join(self._tmp_dir, path)
        if full_path in self._checkouts_by_path:
            raise Exception(f"duplicate mount definition: {full_path}")

        if snapshot is None:
            snapshot = self.default_commit_hash
        if client_name is None:
            client_name = path.replace("/", "_")
        backing_repo_path = (
            backing_repo if backing_repo is not None else self.default_backing_repo
        )

        if mount_protocol is None:
            if sys.platform == "win32":
                mount_protocol = "prjfs"
            else:
                mount_protocol = "fuse"

        state_dir = self.clients_path / client_name
        assert full_path not in self._checkouts_by_path
        config = CheckoutConfig(
            backing_repo=backing_repo_path,
            scm_type=scm_type,
            # pyre-fixme[6]: Expected `str` for 3rd param but got `UUID`.
            guid=uuid.uuid4(),
            mount_protocol=mount_protocol,
            case_sensitive=sys.platform == "linux",
            require_utf8_path=True,
            default_revision=snapshot,
            redirections={},
            redirection_targets={},
            active_prefetch_profiles=[],
            use_write_back_cache=False,
            inode_catalog_type=None,
            off_mount_repo_dir=False,
        )
        checkout = FakeCheckout(state_dir=state_dir, config=config, snapshot=snapshot)
        self._checkouts_by_path[full_path] = checkout

        # Write out the config file and snapshot file
        state_dir.mkdir()
        eden_checkout = EdenCheckout(
            typing.cast(EdenInstance, self), Path(full_path), state_dir
        )
        eden_checkout.save_config(config)
        eden_checkout.save_snapshot(snapshot.encode())

        if active and self._status == fb303_status.ALIVE:
            # Report the mount in /proc/mounts
            dev_id = self._next_dev_id
            self._next_dev_id += 1
            self.mount_table.stats[full_path] = mtab.MTStat(
                st_uid=FAKE_UID, st_dev=dev_id, st_mode=(stat.S_IFDIR | 0o755)
            )

            # Add so that the thrift client will report mount as active
            self.mount_table.add_mount(full_path)

            # Set up directories on disk that look like the mounted checkout
            if setup_path:
                os.makedirs(full_path)
                if scm_type == "hg":
                    self._setup_hg_path(full_path, checkout, dirstate_parent)
                elif scm_type == "git":
                    os.mkdir(os.path.join(full_path, ".git"))

        return EdenCheckout(
            typing.cast(EdenInstance, self), Path(full_path), Path(state_dir)
        )

    def remove_checkout_configuration(self, mount_path: str) -> None:
        """Update the state to make it look like the specified mount path is still
        actively mounted but not configured on disk."""
        checkout = self._checkouts_by_path.pop(mount_path)
        shutil.rmtree(checkout.state_dir)

    def _setup_hg_path(
        self,
        full_path: str,
        fake_checkout: FakeCheckout,
        dirstate_parent: Union[str, Tuple[str, str], None],
    ) -> None:
        hg_dir = Path(full_path) / ".hg"
        hg_dir.mkdir()
        dirstate_path = hg_dir / "dirstate"

        if dirstate_parent is None:
            # The dirstate parent should normally match the snapshot hash
            parents = (binascii.unhexlify(fake_checkout.snapshot), b"\x00" * 20)
        elif isinstance(dirstate_parent, str):
            # Assume we were given a single parent hash as a hex string
            parents = (binascii.unhexlify(dirstate_parent), b"\x00" * 20)
        else:
            # Assume we were given a both parent hashes as hex strings
            parents = (
                binascii.unhexlify(dirstate_parent[0]),
                binascii.unhexlify(dirstate_parent[1]),
            )

        with dirstate_path.open("wb") as f:
            eden.dirstate.write(f, parents, tuples_dict={}, copymap={})

        (hg_dir / "hgrc").write_text("# This file simply needs to exist\n")
        (hg_dir / "requires").write_text("eden\nremotefilelog\nrevlogv1\nstore\n")
        (hg_dir / "sharedpath").write_bytes(
            bytes(fake_checkout.config.backing_repo / ".hg")
        )
        (hg_dir / "shared").write_text("bookmarks\n")
        (hg_dir / "bookmarks").touch()
        (hg_dir / "branch").write_text("default\n")

        source = str(fake_checkout.config.backing_repo)
        fake_repo = FakeHgRepo(source)
        self._hg_repo_by_path[Path(full_path)] = fake_repo
        self._hg_repo_by_path[fake_checkout.config.backing_repo] = fake_repo

    def get_mount_paths(self) -> Iterable[str]:
        return self._checkouts_by_path.keys()

    # TODO: Improve this mock. The real get_mounts() requests info from thrift.
    def get_mounts(self) -> Dict[Path, ListMountInfo]:
        mount_points: Dict[Path, ListMountInfo] = {}
        for strPath, checkout in self._checkouts_by_path.items():
            path = Path(strPath)
            data_dir = checkout.state_dir
            # For mocking purposes, this is being filled in with some default values
            # If you want an actual implementation of get_mounts(), You will need to
            # rewrite this function to determine actual values for ListMountInfo
            mount_points[path] = ListMountInfo(
                path=path,
                data_dir=data_dir,
                state=None,
                configured=True,
                backing_repo=None,
            )
        return mount_points

    def mount(self, path: str, read_only: bool) -> int:
        self.mount_calls.append((path, read_only))
        assert self._status in (
            fb303_status.ALIVE,
            fb303_status.STARTING,
            fb303_status.STOPPING,
        )
        assert path in self._checkouts_by_path
        return 0

    def unmount(
        self, path: str, use_force: bool = True, unintentional_unmount: bool = False
    ) -> None:
        self.unmount_calls.append((path, use_force, unintentional_unmount))
        assert self._status in (
            fb303_status.ALIVE,
            fb303_status.STARTING,
            fb303_status.STOPPING,
        )
        assert path in self._checkouts_by_path

    def check_health(self) -> HealthStatus:
        return HealthStatus(self._status, pid=None, uptime=None, detail="")

    def check_privhelper_connection(self) -> bool:
        return True

    def get_server_build_info(self) -> Dict[str, str]:
        return dict(self._build_info)

    @contextmanager
    def get_thrift_client(
        self, timeout: Optional[float] = None
    ) -> Generator[FakeClient, None, None]:
        """Get a mock thrift client for testing."""
        yield self._fake_client

    def get_checkouts(self) -> List[EdenCheckout]:
        results: List[EdenCheckout] = []
        for mount_path, checkout in self._checkouts_by_path.items():
            results.append(
                EdenCheckout(
                    typing.cast(EdenInstance, self),
                    Path(mount_path),
                    Path(checkout.state_dir),
                )
            )
        return results

    def get_config_value(self, key: str, default: str) -> str:
        return self._config.get(key, default)

    def get_config_bool(self, key: str, default: bool) -> bool:
        if key in self._config:
            return self._config[key] == "true"
        return default

    def get_config_strs(self, key: str, default: configutil.Strs) -> configutil.Strs:
        # pyrefly: ignore [bad-return]
        return self._config.get(key, default)

    def get_hg_repo(self, path: Path) -> FakeHgRepo:
        if path in self._hg_repo_by_path:
            return self._hg_repo_by_path[path]

        def bad_commit_checker(commit: str) -> bool:
            return False

        fake_repo = FakeHgRepo(str(path))
        fake_repo.commit_checker = bad_commit_checker
        return fake_repo

    def log_sample(self, log_type: str, **kwargs: telemetry.TelemetryTypes) -> None:
        self.logged_samples.append((log_type, kwargs))

    def get_running_version_parts(self) -> Tuple[str, str]:
        return (
            self._build_info.get("build_package_version", ""),
            self._build_info.get("build_package_release", ""),
        )

    def get_running_version(self) -> str:
        return version_mod.format_eden_version(self.get_running_version_parts())

    def get_known_bad_edenfs_versions(self) -> Dict[str, List[str]]:
        return {}
