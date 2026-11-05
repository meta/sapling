#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import configparser
import errno
import inspect
import json
import logging
import os
import pathlib
import shutil
import sys
import time
import typing
import unittest
from contextlib import contextmanager
from pathlib import Path
from typing import (
    Any,
    Callable,
    Dict,
    Generator,
    Iterable,
    List,
    Optional,
    Sequence,
    Set,
    Tuple,
    Type,
    Union,
)

import eden.config
from eden.fs.cli import util
from eden.fs.service.eden.thrift_clients import EdenService
from eden.fs.service.eden.thrift_types import (
    FaultDefinition,
    GetBlockedFaultsRequest,
    RemoveFaultArg,
    UnblockFaultArg,
)
from eden.test_support.testcase import EdenTestCaseBase

from . import edenclient, gitrepo, hgrepo, mount_namespace, repobase, skip


mount_namespace.maybe_reexec_in_private_mount_namespace()


class IntegrationTestCase(EdenTestCaseBase):
    pass


@unittest.skipIf(not edenclient.can_run_eden(), "unable to run edenfs")
class EdenTestCase(EdenTestCaseBase):
    """
    Base class for eden integration test cases.

    This starts an eden daemon during setUp(), and cleans it up during
    tearDown().
    """

    mount: str
    eden: edenclient.EdenFS
    start: float
    last_event: float

    # Override enable_fault_injection to True in subclasses to enable Eden's fault
    # injection framework when starting edenfs
    enable_fault_injection: bool = False

    def report_time(self, event: str) -> None:
        """
        report_time() is a helper function for logging how long different
        parts of the test took.

        Each time it is called it logs a message containing the time since the
        test started and the time since the last time report_time() was called.
        """
        now = time.time()
        since_last = now - self.last_event
        since_start = now - self.start
        logging.info("=== %s at %.03fs (+%0.3fs)", event, since_start, since_last)
        self.last_event = now

    def setUp(self) -> None:
        if self.use_io_uring():
            edenclient.require_io_uring_kernel()

        self.start = time.time()
        self.last_event = self.start
        self.system_hgrc: Optional[str] = None

        # Add a cleanup event just to log once the other cleanup
        # actions have completed.
        self.addCleanup(self.report_time, "clean up done")

        super().setUp()

        # Set environment variables to prevent telemetry logging during
        # integration tests.
        self.setenv("EDENFS_INTEGRATION_TEST", "1")
        self.setenv("EDENFS_NO_TELEMETRY", "1")

        # Set this environment variable to enable Sl tracing during the test
        # self.setenv("SL_LOG", "trace")

        # This value seems to work well. Increase it if it's not enough.
        retry_count = 5
        retry_time = 5
        # We are setting the time it takes to free a closed socket
        # to 30 seconds, so this value should match
        max_retry_time = 30

        while retry_count > 0:
            try:
                self.report_time(f"setup_eden with remaining retries {retry_count}")
                self.setup_eden_test()
                self.report_time(
                    f"Done setup_eden with remaining retries {retry_count}"
                )
                break
            except (
                edenclient.EdenCommandError,
                util.EdenStartError,
            ) as e:
                retry_count -= 1
                if retry_count == 0:
                    self.report_time(
                        f"Retries exhausted, failing due to {e.__class__}: {e}"
                    )
                    raise

                retry_time = min(2 * retry_time, max_retry_time)
                self.report_time(
                    f"Failed to start edenfs, retrying in {retry_time} seconds. Error: {e}"
                )

                self.eden.kill(retry=True)
                self.new_tmp_dir()

                time.sleep(retry_time)
            except Exception as e:
                self.report_time(f"Another exception {e.__class__}: {e}")
                raise

        self.report_time("test setup done")

    def tearDown(self) -> None:
        self.report_time("clean up started")
        super().tearDown()

    def setup_eden_test(self) -> None:
        # Place scratch configuration somewhere deterministic for the tests
        scratch_config_file = os.path.join(self.tmp_dir, "scratch.toml")
        with open(scratch_config_file, "w") as f:
            f.write(
                'template = "%s"\n'
                % os.path.join(self.tmp_dir, "scratch").replace("\\", "\\\\")
            )
            f.write("overrides = {}\n")
        self.setenv("SCRATCH_CONFIG_PATH", scratch_config_file)

        # Parent directory for any git/hg repositories created during the test
        self.repos_dir = os.path.join(self.tmp_dir, "repos")
        os.makedirs(self.repos_dir, exist_ok=True)
        # Parent directory for eden mount points
        self.mounts_dir = os.path.join(self.tmp_dir, "mounts")
        os.makedirs(self.mounts_dir, exist_ok=True)
        self.report_time("temporary directory creation done")

        self.eden = self.init_eden_client()

        # Just to better reflect normal user environments, update $HOME
        # to point to our test home directory for the duration of the test.
        self.setenv("HOME", str(self.eden.home_dir))

        self.write_edenfs_system_config()

        # Default to using the Rust version of commands when running
        # integration tests. An empty edenfsctl_rollout file means that all
        # subcommands should use the Rust implementation if available.
        self.set_rust_rollout_config({})

        # Store a lambda in case self.eden is replaced during the test.
        self.addCleanup(lambda: self.eden.cleanup())
        self.eden.start()
        self.report_time("eden daemon started")

        self.mount = os.path.join(self.mounts_dir, "main")

    def init_eden_client(self):
        logging_settings = self.edenfs_logging_settings()
        extra_args = self.edenfs_extra_args()
        if self.enable_fault_injection:
            extra_args.append("--enable_fault_injection")

        return edenclient.EdenFS(
            base_dir=pathlib.Path(self.tmp_dir),
            logging_settings=logging_settings,
            extra_args=extra_args,
            expected_fuse_transport="io_uring" if self.use_io_uring() else "devfuse",
        )

    def write_edenfs_system_config(self) -> None:
        """
        Write edenfs_extra_config() to the system edenfs.rc that the daemon
        reads at startup. A test that changes edenfs_extra_config() between
        two daemons calls this before starting the second one.
        """
        extra_config = dict(self.edenfs_extra_config() or {})
        fuse_config = list(extra_config.get("fuse", []))
        extra_config["fuse"] = fuse_config
        fuse_config.extend(edenclient.fuse_transport_config(self.use_io_uring()))
        self.write_configs(extra_config, self.eden.system_rc_path)

    def write_configs(
        self, config_dict: Dict[str, List[str]], config_file_path
    ) -> None:
        with open(config_file_path, "w") as edenfs_config_file:
            for section_name, lines in config_dict.items():
                edenfs_config_file.write(f"[{section_name}]\n")
                for setting in lines:
                    edenfs_config_file.write(f"{setting}\n")

    @property
    def eden_dir(self) -> str:
        return str(self.eden.eden_dir)

    @property
    def home_dir(self) -> str:
        return str(self.eden.home_dir)

    @property
    def etc_eden_dir(self) -> str:
        return str(self.eden.etc_eden_dir)

    @property
    def mount_path(self) -> pathlib.Path:
        return pathlib.Path(self.mount)

    @property
    def mount_path_bytes(self) -> bytes:
        return bytes(self.mount_path)

    def make_temporary_directory(self, prefix: Optional[str] = None) -> str:
        return str(self.temp_mgr.make_temp_dir(prefix=prefix))

    @contextmanager
    def get_thrift_client(
        self,
    ) -> Generator[EdenService.Sync, None, None]:
        """
        Get a modern thrift-python sync client to the edenfs daemon.
        """
        with self.eden.get_thrift_client() as client:
            yield client

    def get_async_thrift_client(self) -> EdenService.Async:
        """
        Get a thrift client to the edenfs daemon.
        """
        return self.eden.get_async_thrift_client()

    def get_counters(self) -> typing.Mapping[str, float]:
        with self.get_thrift_client() as thrift_client:
            thrift_client.flushStatsNow()
            return thrift_client.getCounters()

    def get_backing_dir(self, reponame: str, repo_type: str) -> Path:
        backing_dir_location = Path(self.repos_dir) / reponame
        return (
            backing_dir_location
            if repo_type in ["hg", "filteredhg"]
            else backing_dir_location / ".git"
        )

    def edenfs_logging_settings(self) -> Optional[Dict[str, str]]:
        """
        Get the log settings to pass to edenfs via the --logging argument.

        This should return a dictionary of {category_name: level}
        - module_name is the C++ log category name.  e.g., "eden.fs.store"
          or "eden.fs.inodes.TreeInode"
        - level is the integer vlog level to use for that module.

        You can return None if you do not want any extra verbose logging
        enabled.
        """
        return None

    def edenfs_extra_args(self) -> List[str]:
        """
        Get additional arguments to pass to edenfs
        """
        return []

    def use_io_uring(self) -> bool:
        return False

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        """
        Get additional configs to write to the edenfs.rc file before starting
        EdenFS.

        The format is the following:
        {"namespace": ["key1=value1", "key2=value2"}
        """
        configs = {
            "experimental": [
                "enable-nfs-server = true",
                "overlay-direct-file-create = true",
                "overlay-cache-wal-files = true",
                'overlay-wal-min-compaction-threshold = "50"',
                "overlay-reuse-created-fds = true",
                "fuse-avoid-write-copy = true",
                "fuse-io-uring-skip-self-wakeup = true",
            ],
            # Defaulting to 8 retry threads is excessive when the test
            # framework runs tests on each CPU core.
            "hg": ['num-retry-threads = "2"'],
            "overlay": [
                'file-prealloc-pool-size = "64"',
                'dir-prealloc-pool-size = "64"',
            ],
            # Keep test daemons out of the production Scuba tables regardless
            # of build mode. EDENFS_INTEGRATION_TEST only reaches the CLIs.
            "telemetry": ['enable-scribe-logging = "false"'],
        }

        # Collect experimental configs from mixins
        experimental_configs = self.get_experimental_configs()
        if experimental_configs:
            configs["experimental"].extend(experimental_configs)

        # Collect coroutines configs from mixins
        coroutines_configs = self.get_coroutines_configs()
        if coroutines_configs:
            configs["coroutines"] = coroutines_configs

        if self.use_nfs():
            configs["clone"] = ['default-mount-protocol = "NFS"']
        # The number of concurrent APFS volumes we can create on macOS
        # Sandcastle hosts is extremely limited. Furthermore, cleaning
        # up disk image redirections on Sandcastle is non-trivial. Let's
        # use symlink. redirections to avoid these issues.
        if sys.platform == "darwin":
            configs["nfs"] = [
                "allow-apple-double = false",
                # On macOS, hard-NFS mounts may hang indefinitely. Use a
                # deadtimeout so that the kernel force unmounts them.
                'dead-timeout-seconds = "30"',
            ]
            if "SANDCASTLE" in os.environ:
                configs["redirections"] = ['darwin-redirection-type = "symlink"']
        elif sys.platform == "win32":
            configs["notifications"] = ['enable-eden-menu = "false"']
        return configs

    def create_hg_repo(
        self,
        name: str,
        hgrc: Optional[configparser.ConfigParser] = None,
        init_configs: Optional[List[str]] = None,
        filtered: bool = False,
    ) -> hgrepo.HgRepository:
        """Create an hg repo.

        Configs used:
        1. Real system config files installed from hg package. See
           `hgrepo.HgRepository.get_system_hgrc_contents()`.
        2. `hgrc`. `hgrc` written after `hg init`.
        3. `init_configs`. Command line flags passed to `hg init`.

        | # | Customizable | Affect `hg init` | Affect commands afterwards |
        --------------------------------------------------------------------
        | 1 | No           | Yes              | Yes                        |
        | 2 | Yes          | No               | Yes                        |
        | 3 | Yes          | Yes              | No                         |
        """
        repo_path = os.path.join(self.repos_dir, name)
        os.makedirs(repo_path, exist_ok=True)

        if self.system_hgrc is None:
            system_hgrc_path = os.path.join(self.repos_dir, "hgrc")
            with open(system_hgrc_path, "w") as f:
                f.write(hgrepo.HgRepository.get_system_hgrc_contents())
            self.system_hgrc = system_hgrc_path

        repo = hgrepo.HgRepository(
            repo_path,
            system_hgrc=self.system_hgrc,
            temp_mgr=self.temp_mgr,
            filtered=filtered,
        )
        repo.init(hgrc=hgrc, init_configs=init_configs)

        return repo

    def create_git_repo(self, name: str) -> gitrepo.GitRepository:
        repo_path = os.path.join(self.repos_dir, name)
        os.makedirs(repo_path, exist_ok=True)
        repo = gitrepo.GitRepository(repo_path, temp_mgr=self.temp_mgr)
        repo.init()

        return repo

    def get_path(self, path: str) -> str:
        """Resolves the path against self.mount."""
        return os.path.join(self.mount, path)

    def touch(self, path: str) -> None:
        """Touch the file at the specified path relative to the clone."""
        fullpath = self.get_path(path)
        with open(fullpath, "a"):
            os.utime(fullpath)

    def write_file(self, path: str, contents: str, mode: int = 0o644) -> None:
        """Create or overwrite a file with the given contents."""
        fullpath = self.get_path(path)
        self.make_parent_dir(fullpath)
        with open(fullpath, "wb") as f:
            f.write(contents.encode())
        os.chmod(fullpath, mode)

    def chmod(self, path: str, mode: int = 0o644) -> None:
        """Create or overwrite a file with the given contents."""
        fullpath = self.get_path(path)
        os.chmod(fullpath, mode)

    def copy(self, from_path: str, to_path: str) -> None:
        """Copy a file/directory at the specified paths relative to the
        clone.
        """
        full_from = self.get_path(from_path)
        full_to = self.get_path(to_path)
        shutil.copy(full_from, full_to)

    def rename(self, from_path: str, to_path: str) -> None:
        """Rename a file/directory at the specified paths relative to the
        clone.
        """
        full_from = self.get_path(from_path)
        full_to = self.get_path(to_path)
        os.rename(full_from, full_to)

    def read_file(self, path: str) -> str:
        """Read the file with the specified path inside the eden repository,
        and return its contents.
        """
        fullpath = self.get_path(path)
        with open(fullpath, "r") as f:
            return f.read()

    def mkdir(self, path: str) -> None:
        """Call mkdir for the specified path relative to the clone."""
        full_path = self.get_path(path)
        try:
            os.makedirs(full_path)
        except OSError as ex:
            if ex.errno != errno.EEXIST:
                raise

    def read_dir(self, path: str) -> List[str]:
        fullpath = self.get_path(path)
        return os.listdir(fullpath)

    def make_parent_dir(self, path: str) -> None:
        dirname = os.path.dirname(path)
        if dirname:
            self.mkdir(dirname)

    def rm(self, path: str) -> None:
        """Unlink the file at the specified path relative to the clone."""
        os.unlink(self.get_path(path))

    def rmdir(self, path: str) -> None:
        """Unlink the directory at the specified path relative to the clone."""
        os.rmdir(self.get_path(path))

    def set_rust_rollout_config(self, config: Dict[str, bool]) -> None:
        """Set the Rust rollout config for this test."""
        with open(self.eden.system_rollout_path, "w") as edenfsctl_rollout:
            edenfsctl_rollout.write(json.dumps(config))

    def stat(self, path: str) -> os.stat_result:
        """Stat the file at the specified path relative to the clone."""
        fullpath = self.get_path(path)
        return os.lstat(fullpath)

    @staticmethod
    def unix_only(fn):
        """
        Decorator that only runs this test on unix platforms.
        """
        if sys.platform == "win32":
            return None
        else:
            return fn

    # TODO(T140123741): add a use_fuse() so we can get rid of the hack to
    # default to NFS on macOS
    def use_nfs(self) -> bool:
        """
        Should this test case mount the repo using NFS. This is used by the
        test replication logic to run our integration tests using the default
        mounting method in addition to NFS. This can not be used to disable
        individual tests from using NFS. Individual tests can be disabled
        from running with NFS via skip lists in eden/integration/lib/skip.py.
        """
        return sys.platform == "darwin"

    def get_experimental_configs(self) -> List[str]:
        """Default implementation returns no additional configs."""
        return []

    def get_coroutines_configs(self) -> List[str]:
        """Default implementation returns no additional coroutines configs."""
        return []

    def remove_fault(
        self,
        keyClass: str,
        keyValueRegex: str = ".*",
    ) -> None:
        with self.eden.get_thrift_client() as client:
            client.removeFault(
                RemoveFaultArg(
                    keyClass=keyClass,
                    keyValueRegex=keyValueRegex,
                )
            )

    def unblock_fault(
        self,
        keyClass: str,
        keyValueRegex: str = ".*",
    ) -> None:
        with self.eden.get_thrift_client() as client:
            client.unblockFault(
                UnblockFaultArg(
                    keyClass=keyClass,
                    keyValueRegex=keyValueRegex,
                )
            )

    def wait_on_fault_unblock(
        self,
        keyClass: str,
        keyValueRegex: str = ".*",
        numToUnblock: int = 1,
    ) -> None:
        def unblock() -> Optional[bool]:
            with self.eden.get_thrift_client() as client:
                unblocked = client.unblockFault(
                    UnblockFaultArg(
                        keyClass=keyClass,
                        keyValueRegex=keyValueRegex,
                    )
                )
            if unblocked == 1:
                return True
            return None

        for _ in range(numToUnblock):
            util.poll_until(unblock, timeout=30)

    def wait_on_fault_hit(self, key_class: str, num_to_hit=1) -> None:
        """
        This waits until we have 'num_to_hit' faults currently blocking.
        """

        def faults_hit() -> Optional[bool]:
            with self.eden.get_thrift_client() as client:
                blocked_faults = client.getBlockedFaults(
                    GetBlockedFaultsRequest(keyclass=key_class)
                ).keyValues
            return True if len(blocked_faults) == num_to_hit else None

        try:
            util.poll_until(faults_hit, timeout=30)
        except TimeoutError as e:
            with self.eden.get_thrift_client() as client:
                # this unblock all faults to avoid tests hang
                client.unblockFault(UnblockFaultArg())
            raise e

    @contextmanager
    def run_with_blocking_fault(
        self,
        keyClass: str,
        keyValueRegex: str = ".*",
    ) -> Generator[None, None, None]:
        with self.eden.get_thrift_client() as client:
            client.injectFault(
                FaultDefinition(
                    keyClass=keyClass,
                    keyValueRegex=keyValueRegex,
                    block=True,
                )
            )

            try:
                yield
            finally:
                client.removeFault(
                    RemoveFaultArg(
                        keyClass=keyClass,
                        keyValueRegex=keyValueRegex,
                    )
                )
                client.unblockFault(
                    UnblockFaultArg(
                        keyClass=keyClass,
                        keyValueRegex=keyValueRegex,
                    )
                )


class EdenRepoTest(EdenTestCase):
    """
    Base class for EdenHgTest and EdenGitTest.

    This sets up a repository and mounts it before starting each test function.

    You normally should put the @eden_repo_test decorator on your test
    when subclassing from EdenRepoTest.  @eden_repo_test will automatically run
    your tests once per supported repository type.
    """

    # pyre-fixme[13]: Attribute `repo` is never initialized.
    repo: repobase.Repository
    # pyre-fixme[13]: Attribute `repo` is never initialized.
    eden_repo: repobase.Repository
    # pyre-fixme[13]: Attribute `repo_name` is never initialized.
    repo_name: str
    # pyre-fixme[13]: Attribute `repo_type` is never initialized.
    repo_type: str
    # pyre-fixme[13]: Attribute `inode_catalog_type` is never initialized.
    inode_catalog_type: str

    # Run Git versions of tests, if on a compatible platform.
    git_test_supported: bool = True

    # Override is_case_sensitive to True in subclasses to force a case-sensitive
    # clone regardless of the OS, or to False to force a case-insensitive clone.
    # Leave this as None to get the current OS's default case-sensitivity
    # setting.
    #
    # The easiest way to use this is to decorate your test class with
    #
    #   @testcase.eden_repo_test(case_sensitivity_dependent=True)
    #
    # which will generate multiple variants of the test with different
    # is_case_sensitive settings - thus allowing you to easily test multiple
    # case sensitivities on a single platform.
    is_case_sensitive: Optional[bool] = None

    backing_store_type: Optional[str] = None

    def setup_eden_test(self) -> None:
        super().setup_eden_test()

        self.repo_name = "main"
        self.inode_catalog_type = "inmemory" if sys.platform == "win32" else "legacy"
        self.repo = self.create_repo(self.repo_name)
        self.populate_repo()
        self.report_time("repository setup done")

        self.eden.clone(
            self.repo.path,
            self.mount,
            case_sensitive=self.is_case_sensitive,
            backing_store=self.backing_store_type,
        )
        self.eden_repo = self.create_eden_repo()
        self.report_time("eden clone done")
        actual_case_sensitive = self.eden.is_case_sensitive(self.mount)
        if self.is_case_sensitive is None:
            self.is_case_sensitive = actual_case_sensitive
        else:
            self.assertEqual(self.is_case_sensitive, actual_case_sensitive)
        self.report_time("eden repo setup done")

    def populate_repo(self) -> None:
        raise NotImplementedError(
            "individual test classes must implement populate_repo()"
        )

    def create_repo(self, name: str) -> repobase.Repository:
        """
        Create a new repository.

        Arguments:
        - name
          The repository name.  This determines the repository location inside
          the self.repos_dir directory.  The full repository path can be
          accessed as repo.path on the returned repo object.
        """
        raise NotImplementedError(
            "test subclasses must implement create_repo().  This is normally"
            " implemented automatically by @eden_repo_test"
        )

    def create_eden_repo(self) -> repobase.Repository:
        """
        Creates a new repository object that refers to the eden client.
        Should be implemented by subclasses is used by the test.
        Implemented automatically by @eden_repo_test (Non-Default)
        """
        raise NotImplementedError(
            "test subclasses must implement create_eden_repo().  This is normally"
            " implemented automatically by @eden_repo_test"
        )

    def assert_checkout_root_entries(
        self,
        expected_entries: Set[str],
        path: Union[str, pathlib.Path, None] = None,
        scm_type: Optional[str] = None,
    ) -> None:
        """Verify that the contents of a checkout root directory are what we expect.

        This automatically expects to find a ".hg" directory in the root of hg
        checkouts.
        """
        checkout_root = pathlib.Path(path if path is not None else self.mount)
        real_scm_type = scm_type if scm_type is not None else self.repo.get_type()
        if real_scm_type in ["hg", "filteredhg"]:
            expected_entries = expected_entries | {".hg"}
        actual_entries = set(os.listdir(checkout_root))
        self.assertEqual(
            expected_entries, actual_entries, f"incorrect entries in {checkout_root}"
        )


def _replicate_test(
    caller_scope: Dict[str, Any],
    replicate: Callable[..., Iterable[Tuple[str, Type[unittest.TestCase]]]],
    test_class: Type[unittest.TestCase],
    args: Sequence[Any],
    kwargs: Dict[str, Any],
) -> None:
    for suffix, new_class in replicate(test_class, *args, **kwargs):
        # Set the name and module information on our new subclass
        name = test_class.__name__ + suffix
        new_class.__name__ = name
        new_class.__qualname__ = name
        new_class.__module__ = test_class.__module__

        def strip_eden_integration_prefix(name: str) -> str:
            prefix = "eden.integration."
            if name.startswith(prefix):
                name = name[len(prefix) :]
            return name

        module = strip_eden_integration_prefix(f"{new_class.__module__}")

        # Allow skipping individual replicated classes, or whole classes.
        class_names = [f"{module}.{name}", f"{module}.{test_class.__name__}"]

        skippedClass = False
        for class_name in class_names:
            if skip.is_class_disabled(class_name):
                skippedClass = True
                break

        if skippedClass:
            # Do not register this class
            continue

        # We also want to be able to skip methods pre replication
        for class_name in class_names:
            for method in dir(new_class):
                if method.startswith("test_"):
                    if skip.is_method_disabled(class_name, method):
                        # A None method will not be listed by unittest causing
                        # the test to never be executed.
                        setattr(new_class, method, None)

        # Add the class to our caller's scope
        caller_scope[name] = new_class


def test_replicator(
    replicate: Callable[..., Iterable[Tuple[str, Type[unittest.TestCase]]]],
) -> Callable[..., Any]:
    """
    A helper function for implementing decorators that replicate TestCase
    classes so that the same test function can be run multiple times with
    several different settings.

    See the @eden_repo_test decorator for an example of how this is used.
    """

    def decorator(
        *args: Any, **kwargs: Any
    ) -> Optional[Callable[[Type[unittest.TestCase]], None]]:
        # We do some rather hacky things here to define new test class types
        # in our caller's scope.  This is needed so that the unittest TestLoader
        # will find the subclasses we define.
        current_frame = inspect.currentframe()
        if current_frame is None:
            raise Exception("we require a python interpreter with stack frame support")
        # pyre-fixme[16]: `Optional` has no attribute `f_locals`.
        caller_scope = current_frame.f_back.f_locals

        if len(args) == 1 and not kwargs and isinstance(args[0], type):
            # The decorator was invoked directly with the test class,
            # with no arguments or keyword arguments
            _replicate_test(caller_scope, replicate, args[0], args=(), kwargs={})
            return None
        else:

            def inner_decorator(test_class: Type[unittest.TestCase]) -> None:
                _replicate_test(caller_scope, replicate, test_class, args, kwargs)

            return inner_decorator

    return decorator


def _replicate_eden_nfs_repo_test(
    test_class: Type[EdenRepoTest],
    run_coroutines: bool = False,
    run_io_uring: bool = True,
) -> Iterable[Tuple[str, Type[EdenRepoTest]]]:
    class CoroRepoTest(CoroutinesTestMixin, test_class):
        pass

    class NFSRepoTest(NFSTestMixin, test_class):
        pass

    class DefaultRepoTest(test_class):
        pass

    variants = [("Default", typing.cast(Type[EdenRepoTest], DefaultRepoTest))]
    # Only run the nfs tests if EdenFS was built with nfs support.
    if eden.config.HAVE_NFS:
        variants.append(("NFS", typing.cast(Type[EdenRepoTest], NFSRepoTest)))

    if run_coroutines:
        variants.append(("Coroutines", typing.cast(Type[EdenRepoTest], CoroRepoTest)))

    result = []
    for label, base in variants:

        class CustomRepoTest(base):
            pass

        result.append((label, typing.cast(Type[EdenRepoTest], CustomRepoTest)))
        # This helper also generates default FUSE variants, despite its NFS name.
        if (
            run_io_uring
            and sys.platform == "linux"
            and not issubclass(base, NFSTestMixin)
        ):

            class IoUringRepoTest(IoUringTestMixin, base):
                pass

            result.append(
                (f"{label}IoUring", typing.cast(Type[EdenRepoTest], IoUringRepoTest))
            )

    return result


# A decorator to duplicate the test to use NFS
#
# Tests that already use eden_repo_test (most of them), do not need to add this
# decorator. However the custom tests that skip this, do need to add this
# decorator.
eden_nfs_repo_test = test_replicator(_replicate_eden_nfs_repo_test)


class WalEnabledMixin:
    """Enables the overlay write-ahead log via `edenfs_extra_config`.

    Used by `eden_nfs_repo_test_with_wal_variant` to generate sibling
    WAL flavors of overlay-sensitive tests so we cover the WAL fast path
    alongside the default (non-WAL) overlay rewrite path.
    """

    # pyre-ignore[15]: Inconsistent override is expected — the mixin
    # composes with EdenRepoTest at runtime via MRO.
    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        # pyre-ignore[16]: `super()` resolves to EdenRepoTest at runtime.
        configs = super().edenfs_extra_config() or {}
        configs.setdefault("overlay", []).append("use-wal = true")
        return configs


def _replicate_eden_nfs_repo_test_with_wal_variant(
    test_class: Type[EdenRepoTest],
    run_io_uring: bool = True,
) -> Iterable[Tuple[str, Type[EdenRepoTest]]]:
    """Variant generator: every `eden_nfs_repo_test` variant, plus a
    WAL flavor of each.

    Yields the base Default/NFS variants unchanged, then re-emits each
    with `WalEnabledMixin` mixed in (suffixed `Wal`). Picks up new
    base variants automatically if `_replicate_eden_nfs_repo_test`
    grows them.
    """
    base_variants = list(
        _replicate_eden_nfs_repo_test(test_class, run_io_uring=run_io_uring)
    )
    # WAL is only implemented for the Legacy FsInodeCatalog
    # (Linux/macOS). Windows uses the InMemory catalog, so the WAL variants
    # would exercise the same code path as the base variants.
    if sys.platform == "win32":
        return base_variants
    wal_variants: List[Tuple[str, Type[EdenRepoTest]]] = []
    for label, cls in base_variants:

        class WalRepoTest(WalEnabledMixin, cls):
            pass

        wal_variants.append(
            (f"{label}Wal", typing.cast(Type[EdenRepoTest], WalRepoTest))
        )
    return base_variants + wal_variants


# Like `eden_nfs_repo_test` but also generates a WAL-on variant for
# every base variant. Apply to overlay-format-sensitive tests where WAL
# changes the on-disk layout (corrupt-overlay recovery, fsck, etc.).
eden_nfs_repo_test_with_wal_variant = test_replicator(
    _replicate_eden_nfs_repo_test_with_wal_variant
)


MixinList = List[Tuple[str, List[Type[Any]]]]


# The complexity check includes the methods of these generated classes.
def _replicate_eden_repo_test(  # noqa: C901
    test_class: Type[EdenRepoTest],
    run_on_nfs: bool = True,
    case_sensitivity_dependent: bool = False,
    run_coroutines: bool = True,
    run_io_uring: bool = True,
) -> Iterable[Tuple[str, Type[EdenRepoTest]]]:
    nfs_variants: MixinList = [("", [])]
    if run_on_nfs and eden.config.HAVE_NFS:
        nfs_variants.append(("NFS", [NFSTestMixin]))

    scm_variants: MixinList = [("Hg", [HgRepoTestMixin])]
    # Gate some tests on whether EdenFS was built to support them.
    if eden.config.HAVE_GIT and test_class.git_test_supported:
        scm_variants.append(("Git", [GitRepoTestMixin]))
    if eden.config.HAVE_FILTEREDHG:
        scm_variants.append(("FilteredHg", [FilteredHgTestMixin]))

    case_variants: MixinList = [("", [])]
    if case_sensitivity_dependent:
        case_variants = [
            ("SystemCaseSensitivity", []),
            ("CaseSensitive", [CaseSensitiveTestMixin]),
            ("CaseInsensitive", [CaseInsensitiveTestMixin]),
        ]

    variants = []
    for nfs_label, nfs_mixins in nfs_variants:
        for scm_label, scm_mixins in scm_variants:
            for case_label, case_mixins in case_variants:

                class VariantRepoTestBase(
                    # pyrefly: ignore [invalid-inheritance]
                    *nfs_mixins,
                    # pyrefly: ignore [invalid-inheritance]
                    *scm_mixins,
                    # pyrefly: ignore [invalid-inheritance]
                    *case_mixins,
                    test_class,
                ):
                    pass

                class VariantRepoTest(VariantRepoTestBase):
                    pass

                variants.append(
                    (
                        f"{nfs_label}{scm_label}{case_label}",
                        typing.cast(Type[EdenRepoTest], VariantRepoTest),
                    )
                )
                if run_io_uring and sys.platform == "linux" and not nfs_mixins:

                    class IoUringVariantRepoTest(IoUringTestMixin, VariantRepoTestBase):
                        pass

                    label = f"{scm_label}{case_label}IoUring"
                    # Keep the original Hg io_uring variant's test names stable.
                    if label == "HgIoUring":
                        label = "IoUring"
                    variants.append(
                        (label, typing.cast(Type[EdenRepoTest], IoUringVariantRepoTest))
                    )

    # Add a single Coroutines variant after all other combinations
    if run_coroutines:

        class CoroutinesRepoTestBase(CoroutinesTestMixin, HgRepoTestMixin, test_class):
            pass

        class CoroutinesVariantRepoTest(CoroutinesRepoTestBase):
            pass

        variants.append(
            ("Coroutines", typing.cast(Type[EdenRepoTest], CoroutinesVariantRepoTest))
        )
        if run_io_uring and sys.platform == "linux":

            class IoUringCoroutinesRepoTest(IoUringTestMixin, CoroutinesRepoTestBase):
                pass

            variants.append(
                (
                    "CoroutinesIoUring",
                    typing.cast(Type[EdenRepoTest], IoUringCoroutinesRepoTest),
                )
            )

    return variants


# A decorator function used to create EdenHgTest and EdenGitTest
# subclasses from a given input test class.
#
# Given an input test class named "MyTest", this will create two separate
# classes named "MyTestHg" and "MyTestGit", which run the tests with
# mercurial and git repositories, respectively.
eden_repo_test = test_replicator(_replicate_eden_repo_test)


class HgRepoTestMixin:
    repo_type: str = "hg"

    def create_repo(self, name: str, filtered: bool = False) -> repobase.Repository:
        # HgRepoTestMixin is always used in classes that derive from EdenRepoTest,
        # but it is difficult to make the type checkers aware of that.  We can't
        # add an abstract create_hg_repo() method to this class since the MRO would find
        # it before the real create_hg_repo() name.  We can't change the MRO without
        # breaking resolution of create_repo().
        # pyre-fixme[16]: `HgRepoTestMixin` has no attribute `create_hg_repo`.
        return self.create_hg_repo(
            name,
            filtered=filtered,
        )

    def create_eden_repo(self) -> repobase.Repository:
        return hgrepo.HgRepository(
            # pyre-fixme[16]: `HgRepoTestMixin` has no attribute `mount`.
            self.mount,
            # pyre-fixme[16]: `HgRepoTestMixin` has no attribute `system_hgrc`.
            system_hgrc=self.system_hgrc,
            # pyre-fixme[16]: `HgRepoTestMixin` has no attribute `backing_store_type`.
            filtered=self.backing_store_type == "filteredhg",
        )


class FilteredHgTestMixin(HgRepoTestMixin):
    backing_store_type: Optional[str] = "filteredhg"

    def create_repo(self, name: str, filtered: bool = True) -> repobase.Repository:
        return super().create_repo(name, filtered=filtered)


class GitRepoTestMixin:
    repo_type: str = "git"

    def create_repo(self, name: str) -> repobase.Repository:
        # pyre-fixme[16]: `GitRepoTestMixin` has no attribute `create_git_repo`.
        return self.create_git_repo(name)

    def create_eden_repo(self) -> repobase.Repository:
        # pyre-fixme[16]: `GitRepoTestMixin` has no attribute `mount`.
        # pyre-fixme[16]: `GitRepoTestMixin` has no attribute `temp_mgr`.
        return gitrepo.GitRepository(self.mount, temp_mgr=self.temp_mgr)


class NFSTestMixin:
    def use_nfs(self) -> bool:
        return True


class IoUringTestMixin:
    def use_io_uring(self) -> bool:
        return True


class CaseSensitiveTestMixin:
    is_case_sensitive = True


class CaseInsensitiveTestMixin:
    is_case_sensitive = False


class CoroutinesTestMixin:
    def get_experimental_configs(self) -> List[str]:
        return [
            "enable-coroutines-debug-get-blob = true",
        ]

    def get_coroutines_configs(self) -> List[str]:
        return [
            "enable-phase7 = true",
            "enable-phase8 = true",
            "enable-phase9 = true",
            "enable-phase10 = true",
        ]


def _replicate_eden_test(
    test_class: Type[unittest.TestCase],
    run_io_uring: bool = False,
) -> Iterable[Tuple[str, Type[unittest.TestCase]]]:
    class EdenTest(test_class):
        pass

    variants = [("Default", typing.cast(Type[unittest.TestCase], EdenTest))]
    if run_io_uring and sys.platform == "linux":

        class IoUringTest(IoUringTestMixin, test_class):
            pass

        variants.append(("IoUring", typing.cast(Type[unittest.TestCase], IoUringTest)))
    return variants


eden_test = test_replicator(_replicate_eden_test)
