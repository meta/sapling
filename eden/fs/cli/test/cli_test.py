#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import argparse
import json
import os
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

import toml
from eden.fs.cli import config as config_mod, main as main_mod, telemetry, util
from eden.fs.cli.config import (
    CheckoutConfig,
    DEFAULT_REVISION,
    EdenCheckout,
    EdenInstance,
)
from eden.fs.service.eden.thrift_types import MountInfo, MountState
from eden.test_support.temporary_directory import TemporaryDirectoryMixin

from .lib.output import TestOutput


class GlobalOptionEnvDefaultsTest(unittest.TestCase):
    def parse_args(self, *args: str) -> argparse.Namespace:
        return main_mod.create_parser().parse_args(list(args))

    def test_global_options_use_env_defaults(self) -> None:
        with patch.dict(
            os.environ,
            {
                "EDENFSCTL_CONFIG_DIR": "/env/config",
                "EDENFSCTL_ETC_EDEN_DIR": "/env/etc",
                "EDENFSCTL_HOME_DIR": "/env/home",
            },
        ):
            args = self.parse_args("--version")

        self.assertEqual(args.config_dir, "/env/config")
        self.assertEqual(args.etc_eden_dir, "/env/etc")
        self.assertEqual(args.home_dir, "/env/home")

    def test_explicit_global_options_override_env_defaults(self) -> None:
        with patch.dict(
            os.environ,
            {
                "EDENFSCTL_CONFIG_DIR": "/env/config",
                "EDENFSCTL_ETC_EDEN_DIR": "/env/etc",
                "EDENFSCTL_HOME_DIR": "/env/home",
            },
        ):
            args = self.parse_args(
                "--config-dir",
                "/flag/config",
                "--etc-eden-dir",
                "/flag/etc",
                "--home-dir",
                "/flag/home",
                "--version",
            )

        self.assertEqual(args.config_dir, "/flag/config")
        self.assertEqual(args.etc_eden_dir, "/flag/etc")
        self.assertEqual(args.home_dir, "/flag/home")

    def test_env_defaults_expand_vars_and_user(self) -> None:
        with patch.dict(
            os.environ,
            {
                "HOME": "/env/home-dir",
                "EDENFSCTL_TEST_ROOT": "/env/root",
                "EDENFSCTL_CONFIG_DIR": "$EDENFSCTL_TEST_ROOT/config",
                "EDENFSCTL_ETC_EDEN_DIR": "~/etc",
                "EDENFSCTL_HOME_DIR": "$EDENFSCTL_TEST_ROOT/home",
            },
        ):
            args = self.parse_args("--version")

        self.assertEqual(args.config_dir, "/env/root/config")
        self.assertEqual(args.etc_eden_dir, "/env/home-dir/etc")
        self.assertEqual(args.home_dir, "/env/root/home")

    def test_explicit_global_options_expand_like_env_defaults(self) -> None:
        with patch.dict(
            os.environ,
            {
                "HOME": "/flag/home-dir",
                "EDENFSCTL_TEST_ROOT": "/flag/root",
                "EDENFSCTL_CONFIG_DIR": "/env/config",
                "EDENFSCTL_ETC_EDEN_DIR": "/env/etc",
                "EDENFSCTL_HOME_DIR": "/env/home",
            },
        ):
            args = self.parse_args(
                "--config-dir",
                "$EDENFSCTL_TEST_ROOT/config",
                "--etc-eden-dir",
                "~/etc",
                "--home-dir",
                "$EDENFSCTL_TEST_ROOT/home",
                "--version",
            )

        self.assertEqual(args.config_dir, "/flag/root/config")
        self.assertEqual(args.etc_eden_dir, "/flag/home-dir/etc")
        self.assertEqual(args.home_dir, "/flag/root/home")

    def test_empty_env_defaults_are_ignored(self) -> None:
        with patch.dict(
            os.environ,
            {
                "EDENFSCTL_CONFIG_DIR": "",
                "EDENFSCTL_ETC_EDEN_DIR": "",
                "EDENFSCTL_HOME_DIR": "",
            },
        ):
            args = self.parse_args("--version")

        self.assertIsNone(args.config_dir)
        self.assertIsNone(args.etc_eden_dir)
        self.assertIsNone(args.home_dir)

    def test_env_default_paths_are_expanded(self) -> None:
        with patch.dict(
            os.environ,
            {
                "EDENFSCTL_CONFIG_DIR": "$EDEN_TEST_BASE/config",
                "EDEN_TEST_BASE": "/expanded/base",
                "HOME": "/home/test",
            },
        ):
            args = self.parse_args("--version")

        self.assertEqual(args.config_dir, "/expanded/base/config")

    def test_explicit_flag_paths_are_expanded(self) -> None:
        with patch.dict(
            os.environ,
            {"EDEN_TEST_BASE": "/expanded/base", "HOME": "/home/test"},
            clear=False,
        ):
            args = self.parse_args("--config-dir", "$EDEN_TEST_BASE/cfg", "--version")

        self.assertEqual(args.config_dir, "/expanded/base/cfg")


class UnmountRedirectionsTest(unittest.TestCase, TemporaryDirectoryMixin):
    def test_uses_global_path_environment_defaults(self) -> None:
        for policy_path in ("home/.edenrc", "etc/edenfs.rc"):
            with self.subTest(policy_path=policy_path):
                root = Path(self.make_temporary_directory()).resolve()
                checkout = root / "checkout"
                client = root / "state" / "clients" / "checkout"
                client.mkdir(parents=True)
                (root / "home").mkdir()
                (root / "etc").mkdir()
                for name in ("remove", "keep"):
                    (checkout / name).mkdir(parents=True)
                    (checkout / name / "file").write_text("contents\n")
                (root / "state" / "config.json").write_text(
                    json.dumps({str(checkout): "checkout"})
                )
                (client / "config.toml").write_text(
                    toml.dumps(
                        {
                            "repository": {"path": str(root / "backing"), "type": "hg"},
                            "redirections": {"remove": "bind", "keep": "bind"},
                        }
                    )
                )
                for path, allowed in ((".edenrc", "keep"), (policy_path, "remove")):
                    (root / path).write_text(
                        toml.dumps(
                            {
                                "redirections": {
                                    "redirect-fixup-deletable-paths": [allowed]
                                }
                            }
                        )
                    )

                with patch.dict(
                    os.environ,
                    {
                        "HOME": str(root),
                        "EDENFSCTL_CONFIG_DIR": str(root / "state"),
                        "EDENFSCTL_ETC_EDEN_DIR": str(root / "etc"),
                        "EDENFSCTL_HOME_DIR": str(root / "home"),
                    },
                ):
                    main_mod.unmount_redirections_for_path(str(checkout), True)

                self.assertFalse((checkout / "remove").exists())
                self.assertEqual("contents\n", (checkout / "keep" / "file").read_text())


class CloneProtocolDefaultTest(unittest.TestCase):
    def test_platform_default(self) -> None:
        cases = (
            ("darwin", True, util.NFS_MOUNT_PROTOCOL_STRING),
            ("linux", False, util.FUSE_MOUNT_PROTOCOL_STRING),
            ("win32", False, util.PRJFS_MOUNT_PROTOCOL_STRING),
        )

        for platform, use_nfs, protocol in cases:
            with (
                self.subTest(platform=platform),
                patch.object(main_mod.sys, "platform", platform),
            ):
                args = main_mod.create_parser().parse_args(["clone", "repo", "path"])
                self.assertEqual(args.nfs, use_nfs)
                self.assertEqual(util.get_protocol(args.nfs), protocol)


class RestartTest(unittest.TestCase):
    def make_restart_cmd(self) -> main_mod.RestartCmd:
        restart_cmd = main_mod.RestartCmd(argparse.ArgumentParser())
        restart_cmd.args = argparse.Namespace(
            allow_root=False,
            daemon_binary=None,
            force_restart=False,
            migrate_to=None,
            only_if_idle=False,
            preserved_vars=None,
            prompt=False,
        )
        return restart_cmd

    def make_telemetry_logger(self) -> telemetry.TestTelemetryLogger:
        telemetry_logger = telemetry.TestTelemetryLogger()
        telemetry_logger.samples = []
        return telemetry_logger

    def test_graceful_restart_falls_back_on_transport_mismatch(self) -> None:
        restart_cmd = self.make_restart_cmd()
        telemetry_logger = self.make_telemetry_logger()
        instance = MagicMock()
        instance.state_dir = Path("/home/test/.eden")
        instance.get_telemetry_logger.return_value = telemetry_logger
        instance.check_health.return_value = MagicMock(pid=1234)
        mismatch = config_mod.FuseTransportMismatch(
            mount=Path("/mnt/eden"),
            active_transport="devfuse",
            desired_transport="io_uring",
        )

        with (
            patch.object(
                config_mod,
                "get_fuse_transport_mismatches",
                return_value=[mismatch],
            ),
            patch.object(
                config_mod,
                "is_fuse_transport_mismatch_restart_enabled",
                return_value=True,
            ),
            patch.object(main_mod, "remove_legacyephemeral_checkouts") as remove_legacy,
            patch.object(restart_cmd, "_full_restart", return_value=0) as full_restart,
        ):
            self.assertEqual(0, restart_cmd._graceful_restart(instance))

        remove_legacy.assert_not_called()
        full_restart.assert_called_once_with(
            instance,
            1234,
            None,
            False,
            False,
        )
        instance.log_sample.assert_called_once_with(
            "full_restart",
            success=True,
            triggered_by="fuse_transport_mismatch",
        )
        self.assertEqual(1, len(telemetry_logger.samples))
        telemetry_sample = telemetry_logger.samples[0]
        self.assertEqual("fuse_transport_mismatch", telemetry_sample.strings["reason"])
        self.assertEqual(
            "devfuse_to_io_uring",
            telemetry_sample.strings["transport_name"],
        )

    def test_graceful_restart_reports_a_refused_takeover(self) -> None:
        restart_cmd = self.make_restart_cmd()
        telemetry_logger = self.make_telemetry_logger()
        instance = MagicMock()
        instance.state_dir = Path("/home/test/.eden")
        instance.get_telemetry_logger.return_value = telemetry_logger
        instance.check_health.return_value = MagicMock(pid=1234)

        with (
            patch.object(config_mod, "get_transport_mismatches", return_value=[]),
            patch.object(main_mod, "remove_legacyephemeral_checkouts"),
            patch.object(
                main_mod.daemon, "gracefully_restart_edenfs_service", return_value=1
            ),
            patch.object(main_mod.daemon, "takeover_was_refused", return_value=True),
            patch.object(main_mod, "wait_for_instance_healthy"),
        ):
            self.assertEqual(
                main_mod.daemon.TAKEOVER_REFUSED_EXIT_CODE,
                restart_cmd._graceful_restart(instance),
            )

        self.assertEqual(1, len(telemetry_logger.samples))

    def test_graceful_restart_falls_back_on_nfs_transport_mismatch(self) -> None:
        restart_cmd = self.make_restart_cmd()
        telemetry_logger = self.make_telemetry_logger()
        instance = MagicMock()
        instance.state_dir = Path("/home/test/.eden")
        instance.get_telemetry_logger.return_value = telemetry_logger
        instance.check_health.return_value = MagicMock(pid=1234)
        mismatch = config_mod.TransportMismatch(
            mount=Path("/mnt/eden"),
            active_transport="tcp",
            desired_transport="unix",
            channel="nfs",
        )

        with (
            patch.object(
                config_mod,
                "is_fuse_transport_mismatch_restart_enabled",
                return_value=False,
            ),
            patch.object(
                config_mod,
                "is_nfs_transport_mismatch_restart_enabled",
                return_value=True,
            ),
            patch.object(
                config_mod,
                "get_nfs_transport_mismatches",
                return_value=[mismatch],
            ),
            patch.object(restart_cmd, "_full_restart", return_value=0),
        ):
            self.assertEqual(0, restart_cmd._graceful_restart(instance))

        instance.log_sample.assert_called_once_with(
            "full_restart",
            success=True,
            triggered_by="nfs_transport_mismatch",
        )
        telemetry_sample = telemetry_logger.samples[0]
        self.assertEqual("nfs_transport_mismatch", telemetry_sample.strings["reason"])
        self.assertEqual("tcp_to_unix", telemetry_sample.strings["transport_name"])

    def test_graceful_restart_skips_transport_check_when_disabled(self) -> None:
        restart_cmd = self.make_restart_cmd()
        telemetry_logger = self.make_telemetry_logger()
        instance = MagicMock()
        instance.state_dir = Path("/home/test/.eden")
        instance.get_telemetry_logger.return_value = telemetry_logger

        with (
            patch.object(
                config_mod,
                "is_fuse_transport_mismatch_restart_enabled",
                return_value=False,
            ),
            patch.object(
                config_mod,
                "is_nfs_transport_mismatch_restart_enabled",
                return_value=False,
            ),
            patch.object(
                config_mod,
                "get_fuse_transport_mismatches",
                side_effect=AssertionError("transport mismatch check should be gated"),
            ) as get_mismatches,
            patch.object(main_mod, "remove_legacyephemeral_checkouts"),
            patch.object(
                main_mod.daemon,
                "gracefully_restart_edenfs_service",
                return_value=0,
            ),
        ):
            self.assertEqual(0, restart_cmd._graceful_restart(instance))

        get_mismatches.assert_not_called()
        self.assertEqual(1, len(telemetry_logger.samples))
        telemetry_sample = telemetry_logger.samples[0]
        self.assertNotIn("reason", telemetry_sample.strings)
        self.assertNotIn("transport_name", telemetry_sample.strings)

    def make_std_stream_mock(self, isatty: bool) -> MagicMock:
        stream = MagicMock()
        stream.isatty.return_value = isatty
        return stream

    def test_full_restart_skips_prompt_when_stdout_is_not_a_tty(self) -> None:
        restart_cmd = self.make_restart_cmd()
        instance = MagicMock()
        stdout_mock = self.make_std_stream_mock(False)

        with (
            patch.object(main_mod.sys, "stdin", self.make_std_stream_mock(True)),
            patch.object(main_mod.sys, "stdout", stdout_mock),
            patch.object(main_mod, "prompt_confirmation") as prompt_confirmation,
            patch.object(restart_cmd, "_do_stop") as do_stop,
            patch.object(
                restart_cmd, "_finish_restart", return_value=0
            ) as finish_restart,
        ):
            self.assertEqual(
                0, restart_cmd._full_restart(instance, 1234, None, True, False)
            )

        prompt_confirmation.assert_not_called()
        do_stop.assert_called_once_with(
            instance, 1234, timeout=main_mod.DEFAULT_STOP_TIMEOUT
        )
        finish_restart.assert_called_once_with(instance, allow_root=False)
        written = "".join(str(c.args[0]) for c in stdout_mock.write.call_args_list)
        self.assertIn("skipping confirmation", written)

    def test_full_restart_prompts_when_stdin_and_stdout_are_ttys(self) -> None:
        restart_cmd = self.make_restart_cmd()
        instance = MagicMock()

        with (
            patch.object(main_mod.sys, "stdin", self.make_std_stream_mock(True)),
            patch.object(main_mod.sys, "stdout", self.make_std_stream_mock(True)),
            patch.object(
                main_mod, "prompt_confirmation", return_value=False
            ) as prompt_confirmation,
            patch.object(restart_cmd, "_do_stop") as do_stop,
        ):
            self.assertEqual(
                1, restart_cmd._full_restart(instance, 1234, None, True, False)
            )

        prompt_confirmation.assert_called_once_with("Proceed?")
        do_stop.assert_not_called()


class ListTest(unittest.TestCase):
    def test_no_mounts(self) -> None:
        out = TestOutput()
        mounts = EdenInstance._combine_mount_info([], [])
        main_mod.ListCmd.print_mounts(out, mounts)
        self.assertEqual(out.getvalue(), "")

    def test_list_mounts_no_backing_repos(self) -> None:
        self.maxDiff = None

        thrift_mounts = [
            MountInfo(
                mountPoint=b"/data/users/johndoe/mercurial",
                edenClientPath=b"/home/johndoe/.eden/clients/mercurial",
                state=MountState.RUNNING,
                visibleInDaemonNamespace=True,
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/git",
                edenClientPath=b"/home/johndoe/.eden/clients/git",
                state=MountState.SHUTTING_DOWN,
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/apache",
                edenClientPath=b"/home/johndoe/.eden/clients/apache",
                state=MountState.RUNNING,
                visibleInDaemonNamespace=False,
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/configs",
                edenClientPath=b"/home/johndoe/.eden/clients/configs",
                state=MountState.INITIALIZING,
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/repos/linux",
                edenClientPath=b"/home/johndoe/.eden/clients/linux",
                state=MountState.RUNNING,
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/other_repos/linux",
                edenClientPath=b"/home/johndoe/.eden/clients/linux2",
                state=MountState.RUNNING,
            ),
        ]
        instance = EdenInstance(
            config_dir="/home/johndoe/.eden",
            etc_eden_dir="/etc/eden",
            home_dir="/home/johndoe",
        )

        checkout1 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/mercurial"),
            Path("/home/johndoe/.eden/clients/mercurial"),
        )
        checkout1.set_config(
            CheckoutConfig(
                backing_repo=Path("/home/johndoe/.eden-backing-repos/mercurial"),
                scm_type="hg",
                guid="123",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["hg"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        checkout2 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/git"),
            Path("/home/johndoe/.eden/clients/git"),
        )
        checkout2.set_config(
            CheckoutConfig(
                backing_repo=Path("/home/johndoe/.eden-backing-repos/git"),
                scm_type="git",
                guid="456",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["git"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        checkout3 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/repos/linux"),
            Path("/home/johndoe/.eden/clients/linux"),
        )
        checkout3.set_config(
            CheckoutConfig(
                backing_repo=Path("/home/johndoe/.eden-backing-repos/linux"),
                scm_type="git",
                guid="789",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["git"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        checkout4 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/other_repos/linux"),
            Path("/home/johndoe/.eden/clients/linux2"),
        )
        checkout4.set_config(
            CheckoutConfig(
                backing_repo=Path("/home/johndoe/.eden-backing-repos/linux"),
                scm_type="git",
                guid="012",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["git"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        checkout5 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/www"),
            Path("/home/johndoe/.eden/clients/www"),
        )
        checkout5.set_config(
            CheckoutConfig(
                backing_repo=Path("/home/johndoe/.eden-backing-repos/www"),
                scm_type="hg",
                guid="345",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["hg"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        config_checkouts = [
            checkout1,
            checkout2,
            checkout3,
            checkout4,
            checkout5,
        ]

        mounts = EdenInstance._combine_mount_info(thrift_mounts, config_checkouts)

        normal_out = TestOutput()
        main_mod.ListCmd.print_mounts(normal_out, mounts)
        self.assertEqual(
            """\
/data/users/johndoe/apache (not visible in daemon namespace) (unconfigured)
/data/users/johndoe/configs (INITIALIZING) (unconfigured)
/data/users/johndoe/git (SHUTTING_DOWN)
/data/users/johndoe/mercurial
/data/users/johndoe/other_repos/linux
/data/users/johndoe/repos/linux
/data/users/johndoe/www (not mounted)
""",
            normal_out.getvalue(),
        )

        json_out = TestOutput()
        main_mod.ListCmd.print_mounts_json(json_out, mounts)
        self.assertEqual(
            """\
{
  "/data/users/johndoe/apache": {
    "backing_repo": null,
    "configured": false,
    "data_dir": "/home/johndoe/.eden/clients/apache",
    "state": "RUNNING",
    "visible_in_daemon_namespace": false
  },
  "/data/users/johndoe/configs": {
    "backing_repo": null,
    "configured": false,
    "data_dir": "/home/johndoe/.eden/clients/configs",
    "state": "INITIALIZING"
  },
  "/data/users/johndoe/git": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/git",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/git",
    "state": "SHUTTING_DOWN"
  },
  "/data/users/johndoe/mercurial": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/mercurial",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/mercurial",
    "state": "RUNNING",
    "visible_in_daemon_namespace": true
  },
  "/data/users/johndoe/other_repos/linux": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/linux",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/linux2",
    "state": "RUNNING"
  },
  "/data/users/johndoe/repos/linux": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/linux",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/linux",
    "state": "RUNNING"
  },
  "/data/users/johndoe/www": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/www",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/www",
    "state": "NOT_RUNNING"
  }
}
""",
            json_out.getvalue(),
        )

    def test_list_mounts_with_backing_repos(self) -> None:
        self.maxDiff = None

        thrift_mounts = [
            MountInfo(
                mountPoint=b"/data/users/johndoe/mercurial",
                edenClientPath=b"/home/johndoe/.eden/clients/mercurial",
                state=MountState.RUNNING,
                backingRepoPath=b"/home/johndoe/.eden-backing-repos/mercurial",
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/git",
                edenClientPath=b"/home/johndoe/.eden/clients/git",
                state=MountState.SHUTTING_DOWN,
                backingRepoPath=None,
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/apache",
                edenClientPath=b"/home/johndoe/.eden/clients/apache",
                state=MountState.RUNNING,
                backingRepoPath=b"/home/johndoe/.eden-backing-repos/apache",
            ),
            MountInfo(
                mountPoint=b"/data/users/johndoe/configs",
                edenClientPath=b"/home/johndoe/.eden/clients/configs",
                state=MountState.INITIALIZING,
            ),
        ]
        instance = EdenInstance(
            config_dir="/home/johndoe/.eden",
            etc_eden_dir="/etc/eden",
            home_dir="/home/johndoe",
        )

        checkout1 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/mercurial"),
            Path("/home/johndoe/.eden/clients/mercurial"),
        )
        checkout1.set_config(
            CheckoutConfig(
                # note the backing repo is never expected to be different in the
                # daemon and client, but for the sake of testing that the
                # backing repo will be taken from the daemon we make them
                # different
                backing_repo=Path("/home/johndoe/.eden-backing-repos/mercurial1"),
                scm_type="hg",
                guid="123",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["hg"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        checkout2 = EdenCheckout(
            instance,
            Path("/data/users/johndoe/git"),
            Path("/home/johndoe/.eden/clients/git"),
        )
        checkout2.set_config(
            CheckoutConfig(
                backing_repo=Path("/home/johndoe/.eden-backing-repos/git"),
                scm_type="git",
                guid="456",
                mount_protocol="fuse",
                case_sensitive=False,
                require_utf8_path=True,
                default_revision=DEFAULT_REVISION["git"],
                redirections={},
                redirection_targets={},
                active_prefetch_profiles=[],
                use_write_back_cache=False,
                re_use_case="buck2-default",
                inode_catalog_type=None,
                off_mount_repo_dir=False,
            )
        )

        config_checkouts = [
            checkout1,
            checkout2,
        ]

        mounts = EdenInstance._combine_mount_info(thrift_mounts, config_checkouts)

        normal_out = TestOutput()
        main_mod.ListCmd.print_mounts(normal_out, mounts)
        self.assertEqual(
            """\
/data/users/johndoe/apache (unconfigured)
/data/users/johndoe/configs (INITIALIZING) (unconfigured)
/data/users/johndoe/git (SHUTTING_DOWN)
/data/users/johndoe/mercurial
""",
            normal_out.getvalue(),
        )

        json_out = TestOutput()
        main_mod.ListCmd.print_mounts_json(json_out, mounts)
        self.assertEqual(
            """\
{
  "/data/users/johndoe/apache": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/apache",
    "configured": false,
    "data_dir": "/home/johndoe/.eden/clients/apache",
    "state": "RUNNING"
  },
  "/data/users/johndoe/configs": {
    "backing_repo": null,
    "configured": false,
    "data_dir": "/home/johndoe/.eden/clients/configs",
    "state": "INITIALIZING"
  },
  "/data/users/johndoe/git": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/git",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/git",
    "state": "SHUTTING_DOWN"
  },
  "/data/users/johndoe/mercurial": {
    "backing_repo": "/home/johndoe/.eden-backing-repos/mercurial",
    "configured": true,
    "data_dir": "/home/johndoe/.eden/clients/mercurial",
    "state": "RUNNING"
  }
}
""",
            json_out.getvalue(),
        )


class ClonePrecheckTest(unittest.TestCase, TemporaryDirectoryMixin):
    def test_clone_into_configured_mount_path(self) -> None:
        tmp_dir = Path(self.make_temporary_directory())
        instance = config_mod.EdenInstance(
            config_dir=tmp_dir / "state",
            etc_eden_dir=tmp_dir / "etc",
            home_dir=tmp_dir / "home",
        )
        mount_path = str(tmp_dir / "mounts" / "repo")
        with patch.object(
            instance,
            "_get_directory_map",
            return_value={Path(mount_path): "repo"},
        ):
            with patch.object(
                instance,
                "_create_mount_point_dir",
                side_effect=RuntimeError("mount point created"),
            ):
                with self.assertRaisesRegex(Exception, "already configured"):
                    instance.clone(MagicMock(), mount_path, "0" * 40)


class FsckThreadCountTest(unittest.TestCase):
    def test_error_discovery_threads_from_config(self) -> None:
        cmd = main_mod.FsckCmd(argparse.ArgumentParser())
        args = argparse.Namespace(
            num_error_discovery_threads=4, check_only=True, force=False
        )
        instance = MagicMock()
        instance.get_config_int.return_value = 16
        with patch.object(main_mod, "get_fsck_command", return_value="eden_fsck"):
            with patch.object(main_mod.subprocess, "call", return_value=0) as call:
                cmd.check_one(args, instance, Path("/checkout"), Path("/state"))
        argv = call.call_args[0][0]
        instance.get_config_int.assert_called_once_with(
            "fsck.num-error-discovery-threads", 4
        )
        self.assertIn("--num_error_discovery_threads=4", argv)
