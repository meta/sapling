#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import configparser
import io
import json
import os
import re
import struct
import sys
import unittest
from collections import namedtuple
from pathlib import Path
from typing import Dict, Union
from unittest.mock import MagicMock, Mock, patch

import toml
import toml.decoder
from eden.fs.cli import configinterpolator
from eden.fs.cli.config import EdenInstance
from eden.test_support.testcase import EdenTestCaseBase

from .. import config as config_mod, configutil, util
from ..configinterpolator import EdenConfigInterpolator
from ..configutil import EdenConfigParser, UnexpectedType

FakeConfigValue = Union[str, bool]


class FakeFuseTransportInstance(config_mod.AbstractEdenInstance):
    def __init__(
        self,
        config: Dict[str, FakeConfigValue],
        mounts: Dict[Path, config_mod.ListMountInfo],
    ) -> None:
        self._config = config
        self._mounts = mounts

    def get_config_value(self, key: str, default: str) -> str:
        value = self._config.get(key, default)
        if isinstance(value, str):
            return value
        section, option = key.split(".", 1)
        raise configutil.UnexpectedType(
            section=section, option=option, value=value, expected_type=str
        )

    def get_config_bool(self, key: str, default: bool) -> bool:
        value = self._config.get(key, default)
        if isinstance(value, bool):
            return value
        section, option = key.split(".", 1)
        raise configutil.UnexpectedType(
            section=section, option=option, value=value, expected_type=bool
        )

    def get_mounts(self) -> Dict[Path, config_mod.ListMountInfo]:
        return self._mounts


def get_toml_test_file_invalid() -> str:
    cfg_file = """
[core thisIsNotAllowed]
"""
    return cfg_file


def get_toml_test_file_defaults() -> str:
    cfg_file = """
[core]
systemIgnoreFile = "/etc/eden/gitignore"
ignoreFile = "/home/${USER}/.gitignore"

[clone]
default-revision = "master"

[rage]
reporter = 'pastry --title "eden rage from $(hostname)"'
"""
    return cfg_file


def get_toml_test_file_user_rc() -> str:
    cfg_file = """
[core]
ignoreFile = "/home/${USER}/.gitignore-override"
edenDirectory = "/home/${USER}/.eden"

["telemetry"]
scribe-cat = "/usr/local/bin/scribe_cat"
"""
    return cfg_file


def get_toml_test_file_dynamic_rc() -> str:
    cfg_file = """
[core]
ignoreFile = "/bad/path/to/.gitignore-override"
"""
    return cfg_file


def get_toml_test_file_system_rc() -> str:
    cfg_file = """
["telemetry"]
scribe-cat = "/bad/path/to/scribe_cat"
"""
    return cfg_file


class TomlConfigTest(EdenTestCaseBase):
    def setUp(self) -> None:
        super().setUp()
        self._user = "bob"
        self._state_dir = self.tmp_dir / ".eden"
        self._etc_eden_dir = self.tmp_dir / "etc/eden"
        self._config_d = self.tmp_dir / "etc/eden/config.d"
        self._home_dir = self.tmp_dir / "home" / self._user
        self._interpolate_dict = {
            "USER": self._user,
            "USER_ID": "42",
            "HOME": str(self._home_dir),
        }

        self._state_dir.mkdir()
        self._config_d.mkdir(exist_ok=True, parents=True)
        self._home_dir.mkdir(exist_ok=True, parents=True)

    def copy_config_files(self) -> None:
        path = self._config_d / "defaults.toml"
        path.write_text(get_toml_test_file_defaults())

        path = self._home_dir / ".edenrc"
        path.write_text(get_toml_test_file_user_rc())

        path = self._etc_eden_dir / "edenfs_dynamic.rc"
        path.write_text(get_toml_test_file_dynamic_rc())

        path = self._etc_eden_dir / "edenfs.rc"
        path.write_text(get_toml_test_file_system_rc())

    def assert_core_config(self, cfg: EdenInstance) -> None:
        self.assertEqual(
            cfg.get_config_value("rage.reporter", default=""),
            'pastry --title "eden rage from $(hostname)"',
        )
        self.assertEqual(
            cfg.get_config_value("core.ignoreFile", default=""),
            f"/home/{self._user}/.gitignore-override",
        )
        self.assertEqual(
            cfg.get_config_value("core.systemIgnoreFile", default=""),
            "/etc/eden/gitignore",
        )
        self.assertEqual(
            cfg.get_config_value("core.edenDirectory", default=""),
            f"/home/{self._user}/.eden",
        )

    def assert_config_precedence(self, cfg: EdenInstance) -> None:
        self.assertEqual(
            cfg.get_config_value("core.ignoreFile", default=""),
            f"/home/{self._user}/.gitignore-override",
        )
        self.assertEqual(
            cfg.get_config_value("telemetry.scribe-cat", default=""),
            "/usr/local/bin/scribe_cat",
        )

    def test_load_config(self) -> None:
        self.copy_config_files()
        cfg = self.get_config()

        # Check the various config sections
        self.assert_core_config(cfg)
        self.assert_config_precedence(cfg)

        # Check if test is for toml or cfg by cfg._user_toml_cfg
        exp_rc_files = [
            self._config_d / "defaults.toml",
            self._etc_eden_dir / "edenfs.rc",
            self._etc_eden_dir / "edenfs_dynamic.rc",
            self._home_dir / ".edenrc",
        ]
        self.assertEqual(cfg.get_rc_files(), exp_rc_files)

    def test_no_dot_edenrc(self) -> None:
        self.copy_config_files()

        (self._home_dir / ".edenrc").unlink()
        cfg = self.get_config()
        cfg._loadConfig()

        self.assertEqual(
            cfg.get_config_value("rage.reporter", default=""),
            'pastry --title "eden rage from $(hostname)"',
        )
        self.assertEqual(
            cfg.get_config_value("core.ignoreFile", default=""),
            "/bad/path/to/.gitignore-override",
        )
        self.assertEqual(
            cfg.get_config_value("core.systemIgnoreFile", default=""),
            "/etc/eden/gitignore",
        )

    def test_toml_error(self) -> None:
        self.copy_config_files()

        self.write_user_config(get_toml_test_file_invalid())

        cfg = self.get_config()
        with self.assertLogs() as logs_assertion:
            cfg._loadConfig()
        self.assertTrue(
            re.search(
                "toml config file .* not valid",
                "\n".join(logs_assertion.output),
            )
        )

    def test_get_config_value_returns_default_if_section_is_missing(self) -> None:
        self.assertEqual(
            self.get_config().get_config_value(
                "missing_section.test_option", default="test default"
            ),
            "test default",
        )

    def test_get_config_value_returns_default_if_option_is_missing(self) -> None:
        self.write_user_config(
            """[test_section]
other_option = "test value"
"""
        )
        self.assertEqual(
            self.get_config().get_config_value(
                "test_section.missing_option", default="test default"
            ),
            "test default",
        )

    def test_get_config_value_returns_value_for_string_option(self) -> None:
        self.write_user_config(
            """[test_section]
test_option = "test value"
"""
        )
        self.assertEqual(
            self.get_config().get_config_value(
                "test_section.test_option", default="test default"
            ),
            "test value",
        )

    def test_user_id_variable_is_set_to_process_uid(self) -> None:
        config = self.get_config_without_stub_variables()
        self.write_user_config(
            """
[testsection]
testoption = "My user ID is ${USER_ID}."
"""
        )

        uid = os.getuid() if sys.platform != "win32" else 0
        self.assertEqual(
            config.get_config_value("testsection.testoption", default=""),
            f"My user ID is {uid}.",
        )

    def test_printed_config_is_valid_toml(self) -> None:
        self.write_user_config(
            """
[clone]
default-revision = "master"
"""
        )

        printed_config = io.BytesIO()
        self.get_config().print_full_config(printed_config)
        parsed_config = printed_config.getvalue().decode("utf-8")
        parsed_toml = toml.loads(parsed_config)

        self.assertIn("clone", parsed_toml)
        self.assertEqual(parsed_toml["clone"].get("default-revision"), "master")

    def test_printed_config_expands_variables(self) -> None:
        self.write_user_config(
            """
["repository fbsource"]
type = "hg"
path = "/data/users/${USER}/fbsource"
"""
        )

        printed_config = io.BytesIO()
        self.get_config().print_full_config(printed_config)

        self.assertIn(b"/data/users/bob/fbsource", printed_config.getvalue())

    def test_printed_config_writes_booleans_as_booleans(self) -> None:
        self.write_user_config(
            """
[prefetch-profiles]
prefetching-enable = true
"""
        )

        printed_config = io.BytesIO()
        self.get_config().print_full_config(printed_config)
        parsed_config = printed_config.getvalue().decode("utf-8")

        self.assertRegex(parsed_config, r"prefetching-enable\s*=\s*true")

    def get_config(self) -> EdenInstance:
        return EdenInstance(
            self._state_dir, self._etc_eden_dir, self._home_dir, self._interpolate_dict
        )

    def get_config_without_stub_variables(self) -> EdenInstance:
        return EdenInstance(
            self._state_dir, self._etc_eden_dir, self._home_dir, interpolate_dict=None
        )

    def write_user_config(self, content: str) -> None:
        (self._home_dir / ".edenrc").write_text(content)

    def test_combine_mount_info_skips_missing_configured_checkout_state(self) -> None:
        cfg = self.get_config()
        checkout = config_mod.EdenCheckout(
            cfg,
            self.tmp_dir / "stale-mount",
            self._state_dir / "clients" / "stale-client",
        )

        mounts = config_mod.EdenInstance._combine_mount_info([], [checkout])

        self.assertEqual({}, mounts)

    def test_combine_mount_info_preserves_invalid_config_errors(self) -> None:
        cfg = self.get_config()
        client_dir = self._state_dir / "clients" / "broken-client"
        client_dir.mkdir(parents=True)
        (client_dir / "config.toml").write_text("not valid toml")
        checkout = config_mod.EdenCheckout(
            cfg,
            self.tmp_dir / "broken-mount",
            client_dir,
        )

        with self.assertRaises(config_mod.CheckoutConfigCorruptedError):
            config_mod.EdenInstance._combine_mount_info([], [checkout])

    def test_retry_removes_client_dir_after_failed_remove(self) -> None:
        client_dir = self._state_dir / "clients" / "half-removed"
        (client_dir / "local").mkdir(parents=True)
        (client_dir / "config.toml").write_text("")
        (client_dir / "SNAPSHOT").write_text("")

        with patch("shutil.rmtree", side_effect=OSError("interrupted")):
            with self.assertRaises(OSError):
                config_mod._remove_client_dir(client_dir)

        self.assertFalse((client_dir / "SNAPSHOT").exists())
        self.assertTrue((client_dir / "config.toml").exists())

        config_mod._remove_client_dir(client_dir)
        self.assertFalse(client_dir.exists())

    def test_remove_client_dir_removes_all_entries(self) -> None:
        client_dir = self._state_dir / "clients" / "removed"
        (client_dir / "local").mkdir(parents=True)
        (client_dir / "local" / "file").write_text("")
        (client_dir / "config.toml").write_text("")
        (client_dir / "SNAPSHOT").write_text("")

        config_mod._remove_client_dir(client_dir)

        self.assertFalse(client_dir.exists())

    def test_register_running_mount_accepts_only_an_unused_client_dir_of_this_instance(
        self,
    ) -> None:
        clients_dir = self._state_dir / "clients"
        orphan = clients_dir / "orphan"
        orphan.mkdir(parents=True)
        (orphan / "config.toml").write_text("")
        registered = clients_dir / "registered"
        registered.mkdir()
        (registered / "config.toml").write_text("")
        no_config = clients_dir / "no-config"
        no_config.mkdir()
        outside = self.tmp_dir / "outside"
        outside.mkdir()
        (outside / "config.toml").write_text("")
        registered_mount = self.tmp_dir / "mount"
        (self._state_dir / "config.json").write_text(
            json.dumps({str(registered_mount): "registered"})
        )
        cfg = self.get_config()
        mount = self.tmp_dir / "orphan-mount"

        self.assertFalse(cfg.can_register_running_mount(mount, registered))
        self.assertFalse(cfg.can_register_running_mount(mount, no_config))
        self.assertFalse(cfg.can_register_running_mount(mount, outside))
        self.assertFalse(cfg.can_register_running_mount(registered_mount, orphan))
        with self.assertRaises(ValueError):
            cfg.register_running_mount(mount, outside)

        self.assertTrue(cfg.can_register_running_mount(mount, orphan))
        cfg.register_running_mount(mount, orphan)
        self.assertEqual(
            {str(registered_mount): "registered", str(mount): "orphan"},
            json.loads((self._state_dir / "config.json").read_text()),
        )

    def test_checkout_without_protocol_defaults_to_nfs_on_macos(self) -> None:
        client_dir = self._state_dir / "clients" / "legacy-client"
        client_dir.mkdir(parents=True)
        (client_dir / "config.toml").write_text(
            """
[repository]
path = "/backing-repo"
type = "hg"
"""
        )

        with patch("sys.platform", "darwin"):
            checkout = config_mod.EdenCheckout(
                self.get_config(),
                self.tmp_dir / "legacy-mount",
                client_dir,
            )
            self.assertEqual("nfs", checkout.get_config().mount_protocol)

    def test_checkout_with_unknown_catalog_type_is_readable(self) -> None:
        client_dir = self._state_dir / "clients" / "sqlite-client"
        client_dir.mkdir(parents=True)
        (client_dir / "config.toml").write_text(
            """
[repository]
path = "/backing-repo"
type = "hg"
inode-catalog-type = "Sqlite"
"""
        )

        checkout = config_mod.EdenCheckout(
            self.get_config(),
            self.tmp_dir / "sqlite-mount",
            client_dir,
        )
        self.assertEqual("sqlite", checkout.get_config().inode_catalog_type)


class SnapshotFileTest(EdenTestCaseBase):
    def write_in_progress_snapshot(
        self, from_component: bytes, to_component: bytes, pid: int
    ) -> Path:
        snapshot_path = self.tmp_dir / "SNAPSHOT"
        snapshot_path.write_bytes(
            util.SNAPSHOT_MAGIC_3
            + struct.pack(">L", pid)
            + struct.pack(">L", len(from_component))
            + from_component
            + struct.pack(">L", len(to_component))
            + to_component
        )
        return snapshot_path

    def test_in_progress_snapshot_with_equal_component_lengths(self) -> None:
        snapshot_path = self.write_in_progress_snapshot(
            util.create_filtered_rootid("a" * 40, b"null"),
            util.create_filtered_rootid("b" * 40, b"null"),
            4242,
        )
        with self.assertRaises(config_mod.InProgressCheckoutError) as context:
            config_mod.get_snapshot(snapshot_path, "filteredhg")
        self.assertEqual("a" * 40, context.exception.from_commit)
        self.assertEqual("b" * 40, context.exception.to_commit)
        self.assertEqual(4242, context.exception.pid)

    def test_in_progress_snapshot_with_different_component_lengths(self) -> None:
        snapshot_path = self.write_in_progress_snapshot(
            util.create_filtered_rootid("a" * 40, b"null"),
            util.create_filtered_rootid("b" * 40, b"V1:filter2"),
            4242,
        )
        with self.assertRaises(config_mod.InProgressCheckoutError) as context:
            config_mod.get_snapshot(snapshot_path, "filteredhg")
        self.assertEqual("a" * 40, context.exception.from_commit)
        self.assertEqual("b" * 40, context.exception.to_commit)
        self.assertEqual(4242, context.exception.pid)


class EdenConfigParserTest(unittest.TestCase):
    unsupported_value = {"dict of string to string": ""}

    def test_loading_config_with_unsupported_type_is_not_an_error(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"test_option": self.unsupported_value}})

    def test_querying_bool_returns_bool(self) -> None:
        for value in [True, False]:
            with self.subTest(value=value):
                parser = EdenConfigParser()
                parser.read_dict({"test_section": {"test_option": value}})
                self.assertEqual(
                    parser.get_bool("test_section", "test_option", default=True), value
                )
                self.assertEqual(
                    parser.get_bool("test_section", "test_option", default=False), value
                )

    def test_querying_bool_with_non_boolean_value_fails(self) -> None:
        for value in ["not a boolean", "", "true", "True", 0]:
            with self.subTest(value=value):
                parser = EdenConfigParser()
                parser.read_dict({"test_section": {"test_option": value}})
                with self.assertRaises(UnexpectedType) as expectation:
                    parser.get_bool("test_section", "test_option", default=False)
                self.assertEqual(expectation.exception.section, "test_section")
                self.assertEqual(expectation.exception.option, "test_option")
                self.assertEqual(expectation.exception.value, value)
                self.assertEqual(expectation.exception.expected_type, bool)

    def test_querying_bool_with_value_of_unsupported_type_fails(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"test_option": self.unsupported_value}})
        with self.assertRaises(UnexpectedType) as expectation:
            parser.get_bool("test_section", "test_option", default=False)
        self.assertEqual(expectation.exception.section, "test_section")
        self.assertEqual(expectation.exception.option, "test_option")
        self.assertEqual(expectation.exception.value, self.unsupported_value)
        self.assertEqual(expectation.exception.expected_type, bool)

    def test_querying_str_with_non_string_value_fails(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"test_option": True}})
        with self.assertRaises(UnexpectedType) as expectation:
            parser.get_str("test_section", "test_option", default="")
        self.assertEqual(expectation.exception.section, "test_section")
        self.assertEqual(expectation.exception.option, "test_option")
        self.assertEqual(expectation.exception.value, True)
        self.assertEqual(expectation.exception.expected_type, str)

    def test_querying_section_str_to_str_returns_mapping(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"a": "a value", "b": "b value"}})
        section = parser.get_section_str_to_str("test_section")
        self.assertCountEqual(section, {"a", "b"})
        self.assertEqual(section["a"], "a value")
        self.assertEqual(section["b"], "b value")

    def test_querying_section_str_to_any_fails_if_option_has_unsupported_type(
        self,
    ) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"unsupported": self.unsupported_value}})
        with self.assertRaises(UnexpectedType) as expectation:
            parser.get_section_str_to_any("test_section")
        self.assertEqual(expectation.exception.section, "test_section")
        self.assertEqual(expectation.exception.option, "unsupported")
        self.assertEqual(expectation.exception.value, self.unsupported_value)
        self.assertIsNone(expectation.exception.expected_type)

    def test_querying_section_str_to_any_interpolates_options(self) -> None:
        parser = EdenConfigParser(
            interpolation=EdenConfigInterpolator({"USER": "alice"})
        )
        parser.read_dict({"test_section": {"test_option": "hello ${USER}"}})
        section = parser.get_section_str_to_any("test_section")
        self.assertEqual(section.get("test_option"), "hello alice")

    def test_querying_section_str_to_any_returns_any_supported_type(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict(
            {
                "test_section": {
                    "bool_option": True,
                    "string_array_option": ["hello", "world"],
                    "string_option": "hello",
                }
            }
        )
        section = parser.get_section_str_to_any("test_section")
        self.assertEqual(section["bool_option"], True)
        self.assertEqual(list(section["string_array_option"]), ["hello", "world"])
        self.assertEqual(section["string_option"], "hello")

    def test_querying_section_str_to_str_with_non_string_value_fails(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"a": False}})
        with self.assertRaises(UnexpectedType) as expectation:
            parser.get_section_str_to_str("test_section")
        self.assertEqual(expectation.exception.section, "test_section")
        self.assertEqual(expectation.exception.option, "a")
        self.assertEqual(expectation.exception.value, False)
        self.assertEqual(expectation.exception.expected_type, str)

    def test_querying_section_str_to_str_of_missing_section_fails(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"a": "a value"}})
        with self.assertRaises(configparser.NoSectionError) as expectation:
            parser.get_section_str_to_str("not_test_section")
        section: str = expectation.exception.section
        self.assertEqual(section, "not_test_section")

    def test_querying_strs_with_empty_array_returns_empty_sequence(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"test_option": []}})
        self.assertEqual(
            list(
                parser.get_strs(
                    "test_section", "test_option", default=["default value"]
                )
            ),
            [],
        )

    def test_querying_strs_with_array_of_strings_returns_strs(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"test_option": ["first", "second", "3rd"]}})
        self.assertEqual(
            list(parser.get_strs("test_section", "test_option", default=[])),
            ["first", "second", "3rd"],
        )

    def test_querying_strs_with_array_of_non_strings_fails(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"test_option": [123]}})
        with self.assertRaises(UnexpectedType) as expectation:
            parser.get_strs("test_section", "test_option", default=[])
        self.assertEqual(expectation.exception.section, "test_section")
        self.assertEqual(expectation.exception.option, "test_option")
        self.assertEqual(expectation.exception.value, [123])
        self.assertEqual(expectation.exception.expected_type, configutil.Strs)

    def test_querying_missing_value_as_strs_returns_default(self) -> None:
        parser = EdenConfigParser()
        parser.read_dict({"test_section": {"bogus_option": []}})
        self.assertEqual(
            list(
                parser.get_strs(
                    "test_section", "missing_option", default=["default value"]
                )
            ),
            ["default value"],
        )

    def test_str_sequences_are_interpolated(self) -> None:
        parser = EdenConfigParser(
            interpolation=EdenConfigInterpolator({"USER": "alice"})
        )
        parser.read_dict(
            {
                "test_section": {
                    "test_option": ["sudo", "-u", "${USER}", "echo", "Hello, ${USER}!"]
                }
            }
        )
        self.assertEqual(
            list(parser.get_strs("test_section", "test_option", default=[])),
            ["sudo", "-u", "alice", "echo", "Hello, alice!"],
        )

    def test_unexpected_type_error_messages_are_helpful(self) -> None:
        self.assertEqual(
            'Expected boolean for telemetry.enable-inodetracebus, but got string: "true"',
            str(
                UnexpectedType(
                    section="telemetry",
                    option="enable-inodetracebus",
                    value="true",
                    expected_type=bool,
                )
            ),
        )

        self.assertEqual(
            "Expected string for repository myrepo.path, but got boolean: true",
            str(
                UnexpectedType(
                    section="repository myrepo",
                    option="path",
                    value=True,
                    expected_type=str,
                )
            ),
        )

        self.assertRegex(
            str(
                UnexpectedType(
                    section="section", option="option", value={}, expected_type=None
                )
            ),
            r"^Unexpected dict for section.option: \{\s*\}$",
        )

        self.assertEqual(
            "Expected array of strings for service.command, but got array: [ 123,]",
            str(
                UnexpectedType(
                    section="service",
                    option="command",
                    value=[123],
                    expected_type=configutil.Strs,
                )
            ),
        )


class EdenInstanceConstructionTest(unittest.TestCase):
    def test_full_cmd_line(self) -> None:
        cmdline = [
            b"/usr/local/libexec/eden/edenfs",
            b"--edenfs",
            b"--edenDir",
            b"/data/users/testuser/.eden",
            b"--etcEdenDir",
            b"/etc/eden",
            b"--configPath",
            b"/home/testuser/.edenrc",
            b"--edenfsctlPath",
            b"/usr/local/bin/edenfsctl",
            b"--takeover",
            b"",
        ]
        instance = config_mod.eden_instance_from_cmdline(cmdline)
        self.assertEqual(instance.state_dir, Path("/data/users/testuser/.eden"))
        self.assertEqual(instance.etc_eden_dir, Path("/etc/eden"))
        self.assertEqual(instance.home_dir, Path("/home/testuser/"))

    @patch("eden.fs.cli.config.EdenInstance.read_configs")
    def test_sparse_cmd_line(self, config: Mock) -> None:
        # We need to mock out the config
        # Otherwise it will prioritize using the user's config on disk
        config.return_value = configutil.EdenConfigParser(
            interpolation=configinterpolator.EdenConfigInterpolator({})
        )
        cmdline = [
            b"/usr/local/libexec/eden/edenfs",
            b"--edenfs",
            b"--etcEdenDir",
            b"/etc/eden",
            b"--configPath",
            b"/home/testuser/.edenrc",
            b"--edenfsctlPath",
            b"/usr/local/bin/edenfsctl",
            b"--takeover",
            b"",
        ]
        instance = config_mod.eden_instance_from_cmdline(cmdline)

        self.assertEqual(
            instance.state_dir, Path("/home/testuser/local/.eden").resolve()
        )
        self.assertEqual(instance.etc_eden_dir, Path("/etc/eden"))
        self.assertEqual(instance.home_dir, Path("/home/testuser/"))

    @patch("eden.fs.cli.config.EdenInstance.read_configs")
    def test_malformed_cmd_line(self, config: Mock) -> None:
        config.return_value = configutil.EdenConfigParser(
            interpolation=configinterpolator.EdenConfigInterpolator({})
        )
        cmdline = [
            b"/usr/local/libexec/eden/edenfs",
            b"--configPath",
            b"/home/testuser/.edenrc",
        ]
        instance = config_mod.eden_instance_from_cmdline(cmdline)

        self.assertEqual(
            instance.state_dir, Path("/home/testuser/local/.eden").resolve()
        )
        self.assertEqual(instance.etc_eden_dir, Path("/etc/eden"))
        self.assertEqual(instance.home_dir, Path("/home/testuser/"))


class FuseTransportMismatchTest(unittest.TestCase):
    def make_instance(
        self,
        config: Dict[str, FakeConfigValue],
        transports: Dict[str, str],
    ) -> FakeFuseTransportInstance:
        mounts = {
            Path(path): config_mod.ListMountInfo(
                path=Path(path),
                data_dir=Path(f"/eden/client{path}"),
                state=None,
                configured=True,
                backing_repo=None,
                fs_channel_type="fuse",
                fuse_transport=transport,
            )
            for path, transport in transports.items()
        }
        return FakeFuseTransportInstance(config, mounts)

    @patch("os.uname")
    def test_detects_devfuse_to_iouring_mismatch(self, mock_uname: MagicMock) -> None:
        FakeUname = namedtuple("FakeUname", ["release"])
        mock_uname.return_value = FakeUname(release="6.13.2-0_fbk7")
        instance = self.make_instance(
            {
                "fuse.use-io-uring": True,
                "fuse.io-uring-kernel-release-regex": r"^6\.13\.",
            },
            {"/mnt/eden": "devfuse"},
        )

        with patch("sys.platform", "linux"):
            mismatches = config_mod.get_fuse_transport_mismatches(instance)

        self.assertEqual(
            [
                config_mod.FuseTransportMismatch(
                    mount=Path("/mnt/eden"),
                    active_transport="devfuse",
                    desired_transport="io_uring",
                )
            ],
            mismatches,
        )

    @patch("os.uname")
    def test_detects_iouring_to_devfuse_mismatch(self, mock_uname: MagicMock) -> None:
        FakeUname = namedtuple("FakeUname", ["release"])
        mock_uname.return_value = FakeUname(release="6.13.2-0_fbk7")
        instance = self.make_instance(
            {"fuse.use-io-uring": False},
            {"/mnt/eden": "io_uring"},
        )

        with patch("sys.platform", "linux"):
            mismatches = config_mod.get_fuse_transport_mismatches(instance)

        self.assertEqual(
            [
                config_mod.FuseTransportMismatch(
                    mount=Path("/mnt/eden"),
                    active_transport="io_uring",
                    desired_transport="devfuse",
                )
            ],
            mismatches,
        )

    def test_nfs_mounts_follow_use_uds(self) -> None:
        instance = self.make_instance(
            {"nfs.use-uds": True, "nfs.restart-on-transport-mismatch": True},
            {},
        )
        instance._mounts = {
            Path("/mnt/eden"): config_mod.ListMountInfo(
                path=Path("/mnt/eden"),
                data_dir=Path("/eden/client/mnt/eden"),
                state=None,
                configured=True,
                backing_repo=None,
                fs_channel_type="nfs3",
                nfs_transport="tcp",
            )
        }

        self.assertEqual(
            [
                config_mod.TransportMismatch(
                    mount=Path("/mnt/eden"),
                    active_transport="tcp",
                    desired_transport="unix",
                    channel="nfs",
                )
            ],
            config_mod.get_nfs_transport_mismatches(instance),
        )
        with patch("sys.platform", "darwin"):
            self.assertEqual(
                config_mod.get_nfs_transport_mismatches(instance),
                config_mod.get_transport_mismatches(instance),
            )

        instance._config["nfs.restart-on-transport-mismatch"] = False
        with patch("sys.platform", "darwin"):
            self.assertEqual([], config_mod.get_transport_mismatches(instance))

    @patch("os.uname")
    def test_disallowed_kernel_desires_devfuse(self, mock_uname: MagicMock) -> None:
        FakeUname = namedtuple("FakeUname", ["release"])
        mock_uname.return_value = FakeUname(release="6.16.1-0_fbk2")
        instance = self.make_instance(
            {
                "fuse.use-io-uring": True,
                "fuse.io-uring-kernel-release-regex": r"^6\.13\.",
            },
            {"/mnt/eden": "devfuse"},
        )

        with patch("sys.platform", "linux"):
            self.assertEqual("devfuse", config_mod.get_desired_fuse_transport(instance))
            self.assertEqual([], config_mod.get_fuse_transport_mismatches(instance))

    def test_non_linux_has_no_desired_fuse_transport(self) -> None:
        instance = self.make_instance(
            {
                "fuse.use-io-uring": True,
                "fuse.io-uring-kernel-release-regex": r"^6\.13\.",
            },
            {"/mnt/eden": "devfuse"},
        )

        with patch("sys.platform", "darwin"):
            self.assertIsNone(config_mod.get_desired_fuse_transport(instance))
            self.assertEqual([], config_mod.get_fuse_transport_mismatches(instance))
