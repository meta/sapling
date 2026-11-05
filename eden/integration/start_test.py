#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import os
import pathlib
import subprocess
import sys
from typing import Dict, List, Optional, Sequence, Tuple

import psutil
from eden.fs.cli.config import EdenInstance
from eden.fs.cli.util import HealthStatus
from fb303_core.thrift_types import fb303_status

from .lib import start, testcase
from .lib.fake_edenfs import get_fake_edenfs_argv
from .lib.find_executables import FindExe
from .lib.service_test_case import service_test, ServiceTestCaseBase


@testcase.eden_test(run_io_uring=True)
class StartTest(testcase.EdenTestCase):
    def test_start_if_necessary(self) -> None:
        # Confirm there are no checkouts configured
        checkouts = self.eden.list_cmd_simple()
        self.assertEqual({}, checkouts)
        self.assertTrue(self.eden.is_healthy())

        # No checkouts and edenfs already running. The no-checkouts case is
        # answered before the health check, so the message names that rather
        # than the running daemon, and the daemon is left alone.
        output = self.eden.run_cmd("start", "--if-necessary")
        self.assertEqual("No EdenFS mount points configured.\n", output)
        self.assertTrue(self.eden.is_healthy())

        # Stop edenfs
        self.eden.shutdown()
        self.assertFalse(self.eden.is_healthy())

        # `eden start --if-necessary` should not start eden
        output = self.eden.run_cmd("start", "--if-necessary")
        self.assertEqual("No EdenFS mount points configured.\n", output)
        self.assertFalse(self.eden.is_healthy())

        # Restart eden and create a checkout
        self.eden.start()
        self.assertTrue(self.eden.is_healthy())

        # Create a repository with one commit
        repo = self.create_hg_repo("testrepo")
        repo.write_file("README", "test\n")
        repo.commit("Initial commit.")
        # Create an Eden checkout of this repository
        checkout_dir = os.path.join(self.mounts_dir, "test_checkout")
        self.eden.clone(repo.path, checkout_dir)

        checkouts = self.eden.list_cmd_simple()
        self.assertEqual({checkout_dir: "RUNNING"}, checkouts)

        # A checkout exists and edenfs is already serving it, so
        # `eden start --if-necessary` has nothing left to do and should succeed.
        output = self.eden.run_cmd("start", "--if-necessary", *self.edenfsctl_args())
        self.assertRegex(output, r"EdenFS is already running \(pid [0-9]+\)\n")
        self.assertTrue(self.eden.is_healthy())

        # Stop edenfs
        self.eden.shutdown()
        self.assertFalse(self.eden.is_healthy())
        # `eden start --if-necessary` should start edenfs now
        if start.eden_start_needs_allow_root_option():
            output = self.eden.run_cmd(
                "start",
                "--if-necessary",
                *self.edenfsctl_args(),
                "--",
                "--allowRoot",
                *self.edenfs_args(),
                capture_stderr=True,
            )
        else:
            output = self.eden.run_cmd(
                "start",
                "--if-necessary",
                *self.edenfsctl_args(),
                "--",
                *self.edenfs_args(),
                capture_stderr=True,
            )
        try:
            self.assertIn("Started EdenFS", output)
            self.assertTrue(self.eden.is_healthy())
            self.eden.assert_running_fuse_transports()
        finally:
            # Raw CLI startup leaves self.eden without a process to clean up.
            self.eden.run_cmd("stop")

    def test_start_if_not_running(self) -> None:
        # EdenFS is already running when the test starts, so
        # `eden start --if-not-running` should have nothing to do
        output = self.eden.run_cmd(
            "start",
            "--if-not-running",
            *self.edenfsctl_args(),
            "--",
            "--allowRoot",
            *self.edenfs_args(),
        )
        self.assertRegex(output, r"EdenFS is already running \(pid [0-9]+\)\n")
        self.assertTrue(self.eden.is_healthy())

        # `eden start` should fail without `--if-not-running`
        proc = self.eden.run_unchecked(
            "start",
            *self.edenfsctl_args(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertRegex(
            proc.stderr, r"error: EdenFS is already running \(pid [0-9]+\)\n"
        )
        self.assertEqual("", proc.stdout)

        # If we stop eden, `eden start --if-not-running` should start it
        self.eden.shutdown()
        self.assertFalse(self.eden.is_healthy())
        self.eden.run_cmd(
            "start",
            "--if-not-running",
            *self.edenfsctl_args(),
            "--",
            "--allowRoot",
            *self.edenfs_args(),
        )
        self.assertTrue(self.eden.is_healthy())

        # Stop edenfs.  We didn't start it through self.eden.start()
        # so the self.eden class doesn't really know it is running and that
        # it needs to be shut down.
        self.eden.run_cmd("stop")

    def test_start_with_preserved_vars(self) -> None:
        self.eden.shutdown()
        self.assertFalse(self.eden.is_healthy())
        self.eden.start(
            preserved_env=["FOO_TEST_VAR", "FOO_TEST_VAR2"],
            extra_env={
                "FOO_TEST_VAR": "foo",
                "FOO_TEST_VAR2": "bar",
                "FOO_UNPRESERVED_TEST_VAR": "baz",
            },
            should_wait_for_daemon_healthy=True,
        )
        self.assertTrue(self.eden.is_healthy())

        daemon_pid = self.eden.get_pid_via_thrift()
        daemon_env = psutil.Process(daemon_pid).environ()
        self.assertEqual(daemon_env["FOO_TEST_VAR"], "foo")
        self.assertEqual(daemon_env["FOO_TEST_VAR2"], "bar")
        self.assertNotIn("FOO_UNPRESERVED_TEST_VAR", daemon_env)

    def edenfsctl_args(self) -> List[str]:
        return ["-d", FindExe.EDEN_DAEMON]

    def edenfs_args(self) -> List[str]:
        args = []

        privhelper = FindExe.EDEN_PRIVHELPER
        if privhelper is not None:
            args.extend(["--privhelper_path", privhelper])

        return args


@testcase.eden_repo_test
class StartWithRepoTest(testcase.EdenRepoTest):
    """Test 'eden start' with a repo and checkout already configured."""

    def test_eden_start_mounts_checkouts(self) -> None:
        self.eden.shutdown()

        with start.run_eden_start_with_real_daemon(
            eden_dir=pathlib.Path(self.eden_dir),
            etc_eden_dir=pathlib.Path(self.etc_eden_dir),
            home_dir=pathlib.Path(self.home_dir),
        ):
            self.assert_checkout_is_mounted()
            self.eden.assert_running_fuse_transports()

    def assert_checkout_is_mounted(self) -> None:
        file = pathlib.Path(self.mount) / "hello"
        self.assertTrue(file.is_file())
        self.assertEqual(file.read_text(), "hola\n")

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")


@testcase.eden_test
class DirectInvokeTest(testcase.IntegrationTestCase):
    def test_eden_cmd_arg(self) -> None:
        """Directly invoking EdenFS with an edenfsctl subcommand should fail."""
        cmd: List[str] = [FindExe.EDEN_DAEMON, "restart"]

        privhelper = FindExe.EDEN_PRIVHELPER
        if privhelper is not None:
            cmd.extend(["--privhelper_path", privhelper])

        out = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertNotEqual(out.returncode, 0)
        self.assertEqual(b"", out.stdout)

        expected_err = "error: unexpected trailing command line arguments\n"
        self.maxDiff = 5000
        self.assertIn(expected_err, out.stderr.decode("utf-8", errors="replace"))


class StartFakeEdenFSTestBase(ServiceTestCaseBase):
    def setUp(self) -> None:
        super().setUp()
        self.eden_dir = self.make_temp_dir("eden")

    def _get_base_eden_args(
        self, eden_dir: Optional[pathlib.Path] = None
    ) -> Tuple[List[str], Dict[str, str]]:
        if eden_dir is None:
            eden_dir = self.eden_dir
        edenfsctl, env = FindExe.get_edenfsctl_env()
        return [
            edenfsctl,
            "--config-dir",
            str(eden_dir),
        ] + self.get_required_eden_cli_args(), env

    def expect_start_failure(
        self,
        msg: str,
        eden_dir: Optional[pathlib.Path] = None,
        extra_args: Optional[Sequence[str]] = None,
    ) -> subprocess.CompletedProcess:
        base_cmd, env = self._get_base_eden_args(eden_dir)
        start_cmd = base_cmd + [
            "start",
            "--daemon-binary",
            FindExe.FAKE_EDENFS,
        ]
        if extra_args:
            start_cmd.extend(extra_args)
        proc = subprocess.run(
            start_cmd,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            errors="replace",
        )

        # Pass through the command output, to make the test easier to debug
        sys.stdout.write(proc.stdout)
        sys.stderr.write(proc.stderr)

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(msg, proc.stderr)
        return proc

    def start_edenfs(
        self,
        eden_dir: Optional[pathlib.Path] = None,
        extra_args: Optional[Sequence[str]] = None,
    ) -> None:
        base_args, env = self._get_base_eden_args(eden_dir)
        start_cmd = base_args + ["start", "--daemon-binary", FindExe.FAKE_EDENFS]
        if extra_args:
            start_cmd.extend(extra_args)
        subprocess.check_call(start_cmd, env=env)

        def cleanup():
            stop_cmd = base_args + ["stop"]
            subprocess.check_call(stop_cmd, env=env)

        self.exit_stack.callback(cleanup)


@service_test
class StartFakeEdenFSTest(StartFakeEdenFSTestBase):
    def test_eden_start_launches_separate_processes_for_separate_eden_dirs(
        self,
    ) -> None:
        eden_dir_1 = self.eden_dir
        eden_dir_2 = self.make_temp_dir("eden2")

        self.start_edenfs(eden_dir=eden_dir_1)
        self.start_edenfs(eden_dir=eden_dir_2)

        instance_1_health: HealthStatus = EdenInstance(
            str(eden_dir_1), etc_eden_dir=None, home_dir=None
        ).check_health()
        self.assertEqual(
            instance_1_health.status,
            fb303_status.ALIVE,
            f"First EdenFS process should be healthy, but it isn't: "
            f"{instance_1_health}",
        )

        instance_2_health: HealthStatus = EdenInstance(
            str(eden_dir_2), etc_eden_dir=None, home_dir=None
        ).check_health()
        self.assertEqual(
            instance_2_health.status,
            fb303_status.ALIVE,
            f"Second EdenFS process should be healthy, but it isn't: "
            f"{instance_2_health}",
        )

        self.assertNotEqual(
            instance_1_health.pid,
            instance_2_health.pid,
            "The EdenFS process should have separate process IDs",
        )

    def test_daemon_command_arguments_should_forward_to_edenfs(self) -> None:
        extra_daemon_args = ["--allowExtraArgs", "--", "hello world", "--ignoredOption"]
        self.start_edenfs(extra_args=["--"] + extra_daemon_args)

        argv = get_fake_edenfs_argv(self.eden_dir)
        expected = [
            "--allowExtraArgs",
            "--foreground",
            "--logPath",
            str(self.eden_dir / "logs/edenfs.log"),
            "--startupLoggerFd",
            "5",
        ] + extra_daemon_args[1:]

        self.assertEqual(
            argv[-len(expected) :],
            expected,
            f"fake_edenfs should have received arguments verbatim\nargv: {argv}",
        )

    def test_daemon_command_arguments_should_forward_to_edenfs_without_leading_dashdash(
        self,
    ) -> None:
        self.start_edenfs(
            extra_args=[
                "hello world",
                "another fake_edenfs argument",
                "--",
                "--allowExtraArgs",
                "arg_after_dashdash",
            ]
        )

        argv = get_fake_edenfs_argv(self.eden_dir)
        expected_extra_daemon_args = [
            "hello world",
            "another fake_edenfs argument",
            "--allowExtraArgs",
            "arg_after_dashdash",
            "--foreground",
            "--logPath",
            str(self.eden_dir / "logs/edenfs.log"),
            "--startupLoggerFd",
            "5",
        ]

        self.assertEqual(
            argv[-len(expected_extra_daemon_args) :],
            expected_extra_daemon_args,
            f"fake_edenfs should have received extra arguments\nargv: {argv}",
        )

    def test_eden_start_resolves_explicit_config_dir_symlinks(self) -> None:
        # Test resolution of symlinks in the Eden state directectory when the
        # --config-dir argument is specified to the Eden CLI.
        link1 = self.tmp_dir / "link1"
        link2 = self.tmp_dir / "link2"
        link1.symlink_to(self.eden_dir, target_is_directory=True)
        link2.symlink_to(link1)
        self._test_eden_start_resolves_config_symlinks(link2, self.eden_dir)

    def test_eden_start_resolves_auto_config_dir_symlinks(self) -> None:
        # Test resolution of symlinks in the Eden state directectory if we don't specify
        # --config-dir and let the Eden CLI automatically figure out the location.
        # This is how Eden normally runs in practice most of the time.
        #
        # Set up symlinks in the home directory location normally used by Eden.
        home_local_dir = self.home_dir / "local"
        data_dir = self.tmp_dir / "data"
        data_dir.mkdir()
        home_local_dir.symlink_to(data_dir, target_is_directory=True)

        resolved_eden_dir = data_dir / ".eden"
        self._test_eden_start_resolves_config_symlinks(None, resolved_eden_dir)

    def _test_eden_start_resolves_config_symlinks(
        self, input_path: Optional[pathlib.Path], resolved_path: pathlib.Path
    ) -> None:
        # Test that the eden CLI resolves symlinks in the Eden state directory path.
        #
        # These must be resolved by the CLI and not the edenfs process: in some cases
        # where the symlinks are on an NFS mount point they can be resolved by the user
        # but not by root.  The edenfs privhelper process runs as root, so it may not be
        # able to resolve these symlinks.  Making sure the symlinks are fully resolved
        # by the CLI enables Eden to still work in these situations.
        if input_path is not None:
            config_dir_args = ["--config-dir", str(input_path)]
        else:
            config_dir_args = []
        edenfsctl, env = FindExe.get_edenfsctl_env()
        fake_edenfs: str = FindExe.FAKE_EDENFS
        base_args = [edenfsctl] + self.get_required_eden_cli_args() + config_dir_args
        start_cmd = base_args + ["start", "--daemon-binary", fake_edenfs]
        stop_cmd = base_args + ["stop"]
        subprocess.check_call(start_cmd, env=env)
        try:
            argv = get_fake_edenfs_argv(resolved_path)
            self.assert_eden_dir(argv, resolved_path)
        finally:
            subprocess.check_call(stop_cmd, env=env)

    def assert_eden_dir(self, argv: List[str], expected: pathlib.Path) -> None:
        try:
            index = argv.index("--edenDir")
        except ValueError:
            self.fail(f"--edenDir not present in arguments: {argv}")
        # pyre-fixme[61]: `index` is undefined, or not always defined.
        actual_config_dir = argv[index + 1]
        self.assertEqual(str(expected), actual_config_dir, f"bad config dir: {argv}")

    def test_eden_start_fails_if_edenfs_is_already_running(self) -> None:
        with self.spawn_fake_edenfs(self.eden_dir) as daemon_pid:
            self.expect_start_failure(f"EdenFS is already running (pid {daemon_pid})")

    def test_eden_start_fails_if_edenfs_fails_during_startup(self) -> None:
        self.expect_start_failure(
            "Started successfully, but reporting failure because "
            "--failDuringStartup was specified",
            extra_args=["--", "--failDuringStartup"],
        )
