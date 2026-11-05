#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import subprocess
import time
from pathlib import Path

from eden.fs.config.eden_config.thrift_types import (
    ConfigReloadBehavior,
    ConfigSourceType,
    EdenConfigData,
)
from eden.fs.service.eden.thrift_types import GetConfigParams

from .lib import testcase


@testcase.eden_test
class ConfigTest(testcase.EdenTestCase):
    def assert_config(
        self,
        config: EdenConfigData,
        name: str,
        value: str,
        sourceType: ConfigSourceType,
    ) -> None:
        actual_value = config.values.get(name)
        self.assertIsNotNone(actual_value)
        assert actual_value is not None  # just to make the type checkers happy
        self.assertEqual(value, actual_value.parsedValue)
        self.assertEqual(sourceType, actual_value.sourceType)

    async def test_get_config(self) -> None:
        self.maxDiff = None

        async with self.get_async_thrift_client() as client:
            # Check the initial config values
            config = await client.getConfig(GetConfigParams())

            # The edenDirectory property is currently always recorded as being set from
            # the command line, regardless of how it was actually determined.
            # (This is to ensure it cannot later be overwritten by a config file change
            # once edenfs has started.)
            self.assert_config(
                config,
                "core:edenDirectory",
                self.eden_dir,
                ConfigSourceType.CommandLine,
            )
            self.assert_config(
                config,
                "core:ignoreFile",
                str(Path(self.home_dir) / ".edenignore"),
                ConfigSourceType.Default,
            )
            self.assert_config(
                config,
                "core:systemIgnoreFile",
                str(Path(self.etc_eden_dir) / "ignore"),
                ConfigSourceType.Default,
            )

            # Update the config on disk
            user_config_path = Path(self.home_dir) / ".edenrc"
            new_ignore_path = Path(self.home_dir) / ".gitignore"
            user_config_path.write_text(
                f"""\
[core]
ignoreFile = "{new_ignore_path}"
"""
            )

            # Get the config, asking just for the cached config values without
            # attempting to reload them from disk.
            config = await client.getConfig(
                GetConfigParams(reload=ConfigReloadBehavior.NoReload)
            )
            # The ignore path should be unchanged
            self.assert_config(
                config,
                "core:ignoreFile",
                str(Path(self.home_dir) / ".edenignore"),
                ConfigSourceType.Default,
            )

            # Now get the config, asking for a reload
            # attempting to reload them from disk.
            config = await client.getConfig(
                GetConfigParams(reload=ConfigReloadBehavior.ForceReload)
            )
            # The ignore path should be updated
            self.assert_config(
                config,
                "core:ignoreFile",
                str(new_ignore_path),
                ConfigSourceType.UserConfig,
            )

    async def test_periodic_reload(self) -> None:
        async with self.get_async_thrift_client() as client:
            await self._test_periodic_reload(client)

    async def _test_periodic_reload(self, client) -> None:
        def write_user_config(reload_interval: str) -> None:
            config_text = f"""
[config]
reload-interval = "{reload_interval}"
"""
            self.eden.user_rc_path.write_text(config_text)

        async def assert_current_interval(expected: str) -> None:
            no_reload_params = GetConfigParams(reload=ConfigReloadBehavior.NoReload)
            config = await client.getConfig(no_reload_params)
            current_interval = config.values["config:reload-interval"].parsedValue
            self.assertEqual(expected, current_interval)

        # By default EdenFS currently automatically reloads
        # the config every 5 minutes
        default_interval = "5m"
        await assert_current_interval(default_interval)

        # Tell EdenFS to reload the config file every 10ms
        write_user_config("10ms")

        # Make EdenFS reload the config immediately to get the new config values
        await client.reloadConfig()
        await assert_current_interval("10ms")

        # Update the reload interval again to 0ms.
        # This tells EdenFS not to auto-reload the config any more.
        write_user_config("0ms")

        # This change should be picked up automatically after ~10ms
        # Sleep for longer than this, then verify the config was updated.
        time.sleep(0.200)
        await assert_current_interval("0ns")

        # Update the reload interval to 6ms.
        # This shouldn't be picked up automatically, since auto-reloads are disabled
        # right now.
        write_user_config("6ms")
        time.sleep(0.200)
        await assert_current_interval("0ns")
        # Force this change to be picked up  now
        await client.reloadConfig()
        await assert_current_interval("6ms")

        # Update the reload interval again.
        # This should be picked up automatically again now that re-enabled the
        # periodic reload interval.
        write_user_config("7ms")
        time.sleep(0.200)
        await assert_current_interval("7ms")

        # If we put a bogus value in the config file it should be ignored,
        # and the normal default (5 minutes) should be used.
        write_user_config("bogus value")
        time.sleep(0.200)
        await assert_current_interval(default_interval)


@testcase.eden_repo_test
class RustRolloutConfigTest(testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def test_fallback_to_rust(self) -> None:
        # Testing the case where we fallback to Rust because a command marked
        # as "experimental" doesn't actually exist in Python.
        self.set_rust_rollout_config({"prefetch-profile": False})
        res = self.eden.run_cmd(
            "prefetch-profile", "list", "--checkout", f"{self.mount}"
        )
        self.assertIn("No active prefetch profiles.", res)

        # If EDENFSCTL_SKIP_RUST is set, we shouldn't try to fallback to Rust
        with self.assertRaises(subprocess.CalledProcessError):
            self.eden.run_cmd(
                "prefetch-profile",
                "list",
                "--checkout",
                f"{self.mount}",
                env={"EDENFSCTL_SKIP_RUST": "1"},
            )

        # If EDENFSCTL_ONLY_RUST is set, ignore the rollout config
        res = self.eden.run_cmd(
            "prefetch-profile",
            "list",
            "--checkout",
            f"{self.mount}",
            env={"EDENFSCTL_ONLY_RUST": "1"},
        )
        self.assertIn("No active prefetch profiles.", res)

        # If EDENFSCTL_SKIP_RUST is set, ignore the rollout config
        self.set_rust_rollout_config({"prefetch-profile": True})
        with self.assertRaises(subprocess.CalledProcessError):
            self.eden.run_cmd(
                "prefetch-profile",
                "list",
                "--checkout",
                f"{self.mount}",
                env={"EDENFSCTL_SKIP_RUST": "1"},
            )
