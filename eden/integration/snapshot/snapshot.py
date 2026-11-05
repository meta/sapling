#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import abc
import contextlib
import datetime
import json
import logging
import os
import re
import socket
import stat
import subprocess
import time
import tomllib
import types
import typing
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, List, Optional, Type, TypeVar, Union

import toml
from eden.integration.lib import edenclient, hgrepo
from eden.integration.lib.temporary_directory import create_tmp_dir
from eden.test_support.temporary_directory import cleanup_tmp_dir

from . import inode_metadata as inode_metadata_mod, verify as verify_mod


T = TypeVar("T", bound="BaseSnapshot")


class BaseSnapshot(metaclass=abc.ABCMeta):
    # The NAME and DESCRIPTION class fields are intended to be overridden on subclasses
    # by the @snapshot_class decorator.
    NAME = "Base Snapshot Class"
    DESCRIPTION = ""

    def __init__(self, base_dir: Path) -> None:
        self.base_dir = base_dir
        # All data inside self.data_dir will be saved as part of the snapshot
        self.data_dir = self.base_dir / "data"
        # Anything inside self.transient_dir will not be saved with the snapshot,
        # and will always be regenerated from scratch when resuming a snapshot.
        self.transient_dir = self.base_dir / "transient"

        self.eden_state_dir = self.data_dir / "eden"

        # We put the etc eden directory inside the transient directory.
        # Whenever we resume a snapshot we want to use a current version of the edenfs
        # daemon and its configuration, rather than an old copy of the edenfs
        # configuration.
        self.etc_eden_dir = self.transient_dir / "etc_eden"

        # We put the home directory inside the transient directory as well.
        self.home_dir = self.transient_dir / "home"

    def __enter__(self: T) -> T:
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        tb: Optional[types.TracebackType],
    ) -> None:
        return None

    @property
    @abc.abstractmethod
    def repo_dir_name(self) -> str:
        """Return the name of the repo state directory."""
        pass

    def create_tarball(self, output_path: Path) -> None:
        """Create a tarball from the snapshot contents.

        Note that in most cases you will likely want to save the snapshot state when
        edenfs is not running, to ensure that the snapshot data is in a consistent
        state.
        """
        # Make sure the output directory exists
        output_path.parent.mkdir(parents=True, exist_ok=True)

        cmd = [
            "gtar",
            "-c",
            "--auto-compress",
            "--sort=name",
            # The inode metadata table usually ends with quite a few empty pages.
            # The --sparse flag allows tar to detect these and avoid emitting them.
            # Given that we normally compress the result this doesn't really make
            # much difference on the final compressed size, though.
            "--sparse",
            # Suppress warnings about the fact that tar skips Eden's socket files.
            "--warning=no-file-ignored",
            # The owner and group IDs in the tar file don't really matter.
            # Just record a fixed data rather than pulling them from the
            # current system being used to generate the archive.
            "--owner=nobody:65534",
            "--group=nobody:65534",
        ] + ["-f", str(output_path), "data"]
        subprocess.check_call(cmd, cwd=self.base_dir)

    def generate(self) -> None:
        """Generate the snapshot data.

        This method should normally be called after constructing the snapshot object
        pointing to an empty directory.
        """
        self._create_directories()
        self._emit_metadata()
        self.gen_before_eden_running()

        # pyrefly: ignore [bad-context-manager]
        with self.edenfs() as eden:
            eden.start()
            self.gen_eden_running(eden)

        self.gen_after_eden_stopped()

        # Rewrite the config state to point to "/tmp/dummy_snapshot_path"
        # This isn't really strictly necessary, but just makes the state that
        # gets saved slightly more deterministic.
        #
        # Also update uid and gid information 99.
        # This is commonly the UID & GID for "nobody" on many systems.
        self._update_eden_state(Path("/tmp/dummy_snapshot_path"), uid=99, gid=99)

    def verify(
        self, verifier: verify_mod.SnapshotVerifier, *, use_io_uring: bool | None = None
    ) -> None:
        """Verify that the snapshot data looks correct.

        This is generally invoked by tests to confirm that an unpacked snapshot still
        works properly with the current version of EdenFS.
        """
        # pyrefly: ignore [bad-context-manager]
        with self.edenfs(use_io_uring=use_io_uring) as eden:
            eden.start()
            print("Verifying snapshot data:")
            print("=" * 60)
            self.verify_snapshot_data(verifier, eden)
            print("=" * 60)

    def edenfs(self, *, use_io_uring: bool | None = None) -> edenclient.EdenFS:
        """Return an EdenFS object that can be used to run an edenfs daemon for this
        snapshot.

        The returned EdenFS object will not be started yet; the caller must explicitly
        call start() on it.

        Tests select a transport explicitly; None preserves the snapshot tools' config.
        """
        if use_io_uring:
            edenclient.require_io_uring_kernel()
        expected_transport = None
        if use_io_uring is not None:
            expected_transport = "io_uring" if use_io_uring else "devfuse"
        eden = edenclient.EdenFS(
            base_dir=self.transient_dir,
            eden_dir=self.eden_state_dir,
            etc_eden_dir=self.etc_eden_dir,
            home_dir=self.home_dir,
            expected_fuse_transport=expected_transport,
        )
        if use_io_uring is not None:
            config = (
                tomllib.loads(eden.system_rc_path.read_text())
                if eden.system_rc_path.exists()
                else {}
            )
            config.setdefault("fuse", {}).update(
                tomllib.loads("\n".join(edenclient.fuse_transport_config(use_io_uring)))
            )
            eden.system_rc_path.write_text(toml.dumps(config))
        return eden

    def resume(self) -> None:
        """Prepare a snapshot to be resumed after unpacking it.

        This updates the snapshot data so it can be run from its new location,
        and recreates any transient state needed for the snapshot.
        """
        self.create_transient_dir()
        self._update_hg_state(self.base_dir)
        self._update_eden_state(self.base_dir, uid=os.getuid(), gid=os.getgid())
        self.prep_resume()

    def _create_directories(self) -> None:
        self.data_dir.mkdir()
        self.create_transient_dir()

    def create_transient_dir(self) -> None:
        self.transient_dir.mkdir()
        self.etc_eden_dir.mkdir()
        self.home_dir.mkdir()

    def _emit_metadata(self) -> None:
        now = time.time()

        # In addition to recording the current time as a unix timestamp,
        # we also store a tuple of (year, month, day).  This is primarily to help make
        # it easier for future verification code if we ever need to alter the
        # verification logic for older versions of the same snapshot type.
        # This will allow more human-readable time comparisons in the code, and makes it
        # easier to compare just based on a prefix of this tuple.
        now_date = datetime.datetime.fromtimestamp(now)
        date_tuple = (
            now_date.year,
            now_date.month,
            now_date.day,
            now_date.hour,
            now_date.minute,
            now_date.second,
        )

        data = {
            "type": self.NAME,
            "description": self.DESCRIPTION,
            "time_created": int(now),
            "date_created": date_tuple,
            "base_dir": str(self.base_dir),
        }
        self._write_metadata(data)

    @property
    def _metadata_path(self) -> Path:
        return self.data_dir / "info.json"

    def _write_metadata(self, data: Dict[str, Any]) -> None:
        with self._metadata_path.open("w") as f:
            json.dump(data, f, indent=2, sort_keys=True)

    def _read_metadata(self) -> Dict[str, Any]:
        with self._metadata_path.open("r") as f:
            return typing.cast(Dict[str, Any], json.load(f))

    def _update_hg_state(self, base_dir: Path) -> None:
        """Update hgrc so that it matches the location of the unpacked snapshot."""
        hgrc_dir = self.data_dir / "repo" / self.repo_dir_name / "hgrc"
        with hgrc_dir.open("r") as f:
            hgrc = f.read()
        match = re.search(r"\/tmp/eden_data\.\w+", hgrc)
        old_dir = match.group(0) if match is not None else None
        if old_dir is not None:
            self._replace_file_contents(hgrc_dir, bytes(Path(old_dir)), bytes(base_dir))
        # The snapshot's remote is an eagerepo at a fixed /tmp path, so give each
        # unpacked snapshot its own to keep concurrent tests from racing to
        # create the same repo.
        match = re.search(r"eager://(/tmp/eden_test\.\w+/eagerepo\w*)", hgrc)
        if match is not None:
            self._replace_file_contents(
                hgrc_dir,
                match.group(1).encode(),
                bytes(self.transient_dir / "eagerepo"),
            )

    def _update_eden_state(self, base_dir: Path, uid: int, gid: int) -> None:
        """Update Eden's stored state for the snapshot so it will work in a new
        location.

        - Replace absolute path names in various data files to refer to the new
          location.  This is needed so that a snapshot originally created in one
          location can be unpacked and used in another location.

        - Update UID and GID values stored by Eden's to reflect the specified values.
          This is needed so that unpacked snapshots can be used by the current user
          without getting permissions errors when they try to access files inside the
          Eden checkouts.
        """
        info = self._read_metadata()
        old_base_dir = Path(info["base_dir"])

        # A few files in the RocksDB directory end up with the absolute path
        # embedded in them.
        rocks_db_path = self.eden_state_dir / "storage" / "rocks-db"
        for entry in rocks_db_path.iterdir():
            if entry.name.startswith("LOG") or entry.name.startswith("OPTIONS"):
                self._replace_file_contents(entry, bytes(old_base_dir), bytes(base_dir))

        # Parse eden's config.json to get the list of checkouts, and update each one.
        eden_config_path = self.eden_state_dir / "config.json"
        with eden_config_path.open("r+") as config_file:
            eden_data = json.load(config_file)
            new_config_data = {}
            for _old_checkout_path, checkout_name in eden_data.items():
                new_checkout_path = self.data_dir / checkout_name
                new_config_data[str(new_checkout_path)] = checkout_name
                checkout_state_dir = self.eden_state_dir / "clients" / checkout_name
                self._relocate_checkout(checkout_state_dir, old_base_dir, base_dir)
                self._update_ownership(checkout_state_dir, uid, gid)

            config_file.seek(0)
            config_file.truncate()
            json.dump(new_config_data, config_file, indent=2, sort_keys=True)

        # Update the info file with the new base path
        info["base_dir"] = str(base_dir)
        self._write_metadata(info)

    def _update_ownership(self, checkout_state_dir: Path, uid: int, gid: int) -> None:
        """Update Eden's stored metadata about files to mark that files are owned by
        the current user."""
        metadata_path = checkout_state_dir / "local" / "metadata.table"
        inode_metadata_mod.update_ownership(metadata_path, uid, gid)

    def _relocate_checkout(
        self, checkout_state_dir: Path, old_base_dir: Path, new_base_dir: Path
    ) -> None:
        self._replace_file_contents(
            checkout_state_dir / "config.toml", bytes(old_base_dir), bytes(new_base_dir)
        )
        overlay_dir = checkout_state_dir / "local"
        self._relocate_overlay_dir(
            overlay_dir, bytes(old_base_dir), bytes(new_base_dir)
        )

    def _relocate_overlay_dir(
        self, dir_path: Path, old_data: bytes, new_data: bytes
    ) -> None:
        # Recursively update the contents for every file in the overlay
        # if it contains the old path.
        #
        # This approach is pretty dumb: we aren't processing the overlay file formats at
        # all, just blindly replacing the contents if we happen to see something that
        # looks like the old path.  For now this is the easiest thing to do, and the
        # chance of other data looking like the source path should be very unlikely.
        #
        # The following files typically contain absolute paths to the original
        # snapshot location:
        #   .eden/root
        #   .eden/client
        #   .eden/socket
        #   .hg/sharedpath
        #
        # Everything in the .eden/ directory will be updated automatically by EdenFS
        # when the checkout is mounted.  Nonetheless it still seems good to update these
        # files with a generic path after we shut down EdenFS when first generating the
        # snapshot data.
        #
        for path in dir_path.iterdir():
            stat_info = path.lstat()
            if stat.S_ISDIR(stat_info.st_mode):
                self._relocate_overlay_dir(path, old_data, new_data)
            else:
                self._replace_file_contents(path, old_data, new_data)

    def _replace_file_contents(
        self, path: Path, old_data: bytes, new_data: bytes
    ) -> None:
        with path.open("rb+") as f:
            file_contents = f.read()
            new_contents = file_contents.replace(old_data, new_data)
            if new_contents != file_contents:
                f.seek(0)
                f.truncate()
                f.write(new_contents)

    def gen_before_eden_running(self) -> None:
        """gen_before_eden_running() will be called when generating a new snapshot after
        the directory structure has been set up but before edenfs is started.

        Subclasses of BaseSnapshot can perform any work they want here.
        """
        return None

    def gen_eden_running(self, eden: edenclient.EdenFS) -> None:
        """gen_eden_running() will be called when generating a new snapshot once edenfs
        has been started.

        Subclasses of BaseSnapshot can perform any work they want here.
        """
        return None

    def gen_after_eden_stopped(self) -> None:
        """gen_after_eden_stopped() will be called as the final step of generating a
        snapshot, once edenfs has been stopped.

        Subclasses of BaseSnapshot can perform any work they want here.
        """
        return None

    def prep_resume(self) -> None:
        """prep_resume() will be when preparing to resume a snapshot, before edenfs has
        been started.

        Subclasses of BaseSnapshot can perform any work they want here.
        here.
        """
        return None

    @abc.abstractmethod
    def verify_snapshot_data(
        self, verifier: verify_mod.SnapshotVerifier, eden: edenclient.EdenFS
    ) -> None:
        """Verify that the snapshot data looks correct.

        This method should be overridden by subclasses.
        """
        pass


class HgSnapshot(BaseSnapshot, metaclass=abc.ABCMeta):
    """A helper parent class for BaseSnapshot implementations that creates a single
    checkout of a mercurial repository."""

    system_hgrc_path: Path
    backing_repo: hgrepo.HgRepository

    def create_transient_dir(self) -> None:
        super().create_transient_dir()

        # Note that we put the system hgrc file in self.transient_dir rather than
        # self.data_dir:
        # This file is not saved with the snapshot, and is instead regenerated each time
        # we unpack the snapshot.  This reflects the fact that we always run with the
        # current system hgrc rather than an old snapshot of the system configs.
        self.system_hgrc_path = self.transient_dir / "system_hgrc"
        self.system_hgrc_path.write_text(hgrepo.HgRepository.get_system_hgrc_contents())

    def hg_repo(self, path: Path) -> hgrepo.HgRepository:
        return hgrepo.HgRepository(str(path), system_hgrc=str(self.system_hgrc_path))

    def gen_before_eden_running(self) -> None:
        logging.info("Creating backing repository...")
        # Create the repository
        backing_repo_path = self.data_dir / "repo"
        backing_repo_path.mkdir()
        self.backing_repo = self.hg_repo(backing_repo_path)
        self.backing_repo.init()

        self.populate_backing_repo()

    def gen_eden_running(self, eden: edenclient.EdenFS) -> None:
        logging.info("Preparing checkout...")

        eden.clone(self.backing_repo.path, str(self.checkout_path))

        # pyre-fixme[16]: `HgSnapshot` has no attribute `checkout_repo`.
        self.checkout_repo = self.hg_repo(self.checkout_path)
        self.populate_checkout()

    @abc.abstractmethod
    def populate_backing_repo(self) -> None:
        pass

    @abc.abstractmethod
    def populate_checkout(self) -> None:
        pass

    @property
    def checkout_path(self) -> Path:
        """Return the path to the checkout root."""
        return self.data_dir / "checkout"

    @property
    def repo_dir_name(self) -> str:
        """Return the name of the repo state directory."""
        # TODO: Conditionally use ".hg" or ".sl" based on OSS or internal repo
        return ".hg"

    @property
    def checkout_scm_dir(self) -> Path:
        """Return the path to the checkout's .hg directory."""
        return self.checkout_path / self.repo_dir_name

    def read_file(self, path: Union[Path, str]) -> bytes:
        """Helper function to read a file in the checkout.
        This is primarily used to ensure that the file is loaded.
        """
        file_path = self.checkout_path / path
        with file_path.open("rb") as f:
            data: bytes = f.read()
        return data

    def write_file(
        self, path: Union[Path, str], contents: bytes, mode: int = 0o644
    ) -> None:
        """Helper function to write a file in the checkout."""
        file_path = self.checkout_path / path
        file_path.parent.mkdir(parents=True, exist_ok=True)
        with file_path.open("wb") as f:
            os.fchmod(f.fileno(), mode)
            f.write(contents)

    def symlink(self, path: Union[Path, str], contents: bytes) -> None:
        """Helper function to create or update a symlink in the checkout."""
        file_path = self.checkout_path / path
        try:
            file_path.unlink()
        except FileNotFoundError:
            file_path.parent.mkdir(parents=True, exist_ok=True)
        os.symlink(contents, bytes(file_path))

    def chmod(self, path: Union[Path, str], mode: int) -> None:
        file_path = self.checkout_path / path
        os.chmod(file_path, mode)

    def mkdir(self, path: Union[Path, str], mode: int = 0o755) -> None:
        dir_path = self.checkout_path / path
        dir_path.mkdir(mode=mode, parents=True, exist_ok=False)
        # Explicitly call chmod() to ignore any umask settings
        dir_path.chmod(mode)

    def list_dir(self, path: Union[Path, str]) -> List[Path]:
        """List the contents of a directory in the checkout.
        This can be used to ensure the directory has been loaded by Eden.
        """
        dir_path = self.checkout_path / path
        return list(dir_path.iterdir())

    def make_socket(self, path: Union[Path, str], mode: int = 0o755) -> None:
        socket_path = self.checkout_path / path
        socket_path.parent.mkdir(parents=True, exist_ok=True)
        with socket.socket(socket.AF_UNIX) as sock:
            # Call fchmod() before we create the socket to ensure that its initial
            # permissions are not looser than requested.  The OS will still honor the
            # umask when creating the socket.
            os.fchmod(sock.fileno(), mode)
            sock.bind(str(socket_path))
            sock.listen(10)
            # Call chmod() update the permissions ignoring the umask.
            # Note that we unfortunately must use path.chmod() here rather than
            # os.fchmod(): Linux appears to ignore fchmod() calls after the socket has
            # already been bound.
            socket_path.chmod(mode)


snapshot_types: Dict[str, Type[BaseSnapshot]] = {}


def snapshot_class(
    name: str, description: str
) -> Callable[[Type[BaseSnapshot]], Type[BaseSnapshot]]:
    """A decorator for registering snapshot implementations."""

    def wrapper(snapshot: Type[BaseSnapshot]) -> Type[BaseSnapshot]:
        snapshot.NAME = name
        snapshot.DESCRIPTION = description
        snapshot_types[name] = snapshot
        return snapshot

    return wrapper


@contextlib.contextmanager
def generate(snapshot_type: Type[T]) -> Iterator[T]:
    """Generate a snapshot using the specified snapshot type.

    The argument must be a subclass of BaseSnapshot.
    This should be used in a `with` statement.  This method generates the snapshot in a
    temporary directory that will be cleaned up when exiting the `with` context.
    """
    with create_tmp_dir() as tmpdir:
        snapshot = snapshot_type(tmpdir)
        snapshot.generate()
        yield snapshot


class UnknownSnapshotTypeError(ValueError):
    def __init__(self, type_name: str) -> None:
        super().__init__(f"unknown snapshot type {type_name!r}")
        self.type_name = type_name


def unpack_into(snapshot_path: Path, output_path: Path) -> BaseSnapshot:
    """Unpack a snapshot into the specified output directory.

    Returns the appropriate BaseSnapshot subclass for this snapshot.
    """
    # GNU tar is smart enough to automatically figure out the correct
    # decompression method.
    untar_cmd = ["tar", "-xf", str(snapshot_path.absolute())]
    subprocess.check_call(untar_cmd, cwd=output_path)

    data_dir = output_path / "data"
    try:
        with (data_dir / "info.json").open("r") as info_file:
            info = json.load(info_file)

        type_name = info["type"]
        snapshot_type = snapshot_types.get(type_name)
        if snapshot_type is None:
            raise UnknownSnapshotTypeError(type_name)

        # pyre-fixme[45]: Cannot instantiate abstract class `BaseSnapshot`.
        snapshot = snapshot_type(output_path)
        snapshot.resume()
        return snapshot
    except Exception:
        cleanup_tmp_dir(data_dir)
        raise


def _import_snapshot_modules() -> None:
    import __manifest__

    # Find and import all modules in our "types" sub-package.
    # Each module will register its snapshot types when imported.
    package_prefix = f"{__package__}.types."
    for module in __manifest__.modules:
        if module.startswith(package_prefix):
            __import__(module)


def get_snapshots_root() -> Path:
    """
    Return the path where snapshots are stored.
    """
    root = os.environ.get("EDENFS_SNAPSHOTS", "eden/test-data")
    return Path(root).resolve() / "snapshots"


# Automatically import all snapshot modules to register their snapshot classes
_import_snapshot_modules()
