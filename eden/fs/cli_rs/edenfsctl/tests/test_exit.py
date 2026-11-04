# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import json
import os
import subprocess
import tempfile
import unittest


class EdenfsctlExitTest(unittest.TestCase):
    """How `edenfsctl` leaves the process: exit code and output on every path.

    The binary runs against an empty, isolated config directory, so no daemon
    is reached, and against a fresh `scm_use_cases` cache, so no remote config
    is fetched.
    """

    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory(ignore_cleanup_errors=True)
        self.addCleanup(self.dir.cleanup)
        self.binary = os.path.abspath(os.environ["EDENFSCTL"])
        self.config_dir = os.path.join(self.dir.name, "eden")
        os.mkdir(self.config_dir)
        with open(os.path.join(self.config_dir, "scm_use_cases"), "w") as f:
            json.dump({"use_cases": {}}, f)

    def run_eden(
        self, *args: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        full_env = dict(os.environ, EDENFS_NO_TELEMETRY="1", EDENFSCTL_ONLY_RUST="1")
        full_env.pop("PYTHONHOME", None)
        full_env.pop("PYTHONPATH", None)
        full_env.update(env or {})
        return subprocess.run(
            [self.binary, "--config-dir", self.config_dir, *args],
            env=full_env,
            capture_output=True,
            text=True,
        )

    def test_success_writes_its_output_and_exits_zero(self) -> None:
        done = self.run_eden("list", "--json")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {})

    def test_error_prints_the_error_and_exits_one(self) -> None:
        with open(os.path.join(self.config_dir, "config.json"), "w") as f:
            f.write("not json\n")
        done = self.run_eden("list", "--json")
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertEqual(done.stdout, "")
        self.assertTrue(
            done.stderr.startswith("Error: Failed to parse directory map"),
            done.stderr,
        )
