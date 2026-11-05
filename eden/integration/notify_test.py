#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import asyncio
import json
import os
import platform
import subprocess
import sys
import time
import typing as t
from typing import Any

from .lib import testcase, util


@testcase.eden_repo_test
class NotifyTest(testcase.EdenRepoTest):
    initial_commit: str = ""

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.initial_commit = self.repo.commit("Initial commit.")

    def setUp(self) -> None:
        super().setUp()
        if platform.system() == "Windows" and not getattr(self, "loop", None):
            # This is required on Windows
            # pyre-ignore[16]: Windows only
            self.loop = asyncio.ProactorEventLoop()
            asyncio.set_event_loop(self.loop)

    def tearDown(self) -> None:
        if getattr(self, "loop", None):
            self.loop.close()
        super().tearDown()

    async def subscribe(self, *args) -> asyncio.subprocess.Process:
        self.mkdir(".edenfs-notifications-state")
        # Wait to prevent the file creation from showing up in the stream
        time.sleep(1)
        cmd, env = self.eden.get_edenfsctl_cmd_env(
            "notify",
            "changes-since",
            "--subscribe",
            "--throttle",
            "5",
            "--json",
            self.mount,
            *args,
        )
        env["EDENFS_LOG"] = "edenfs=trace"

        return await asyncio.create_subprocess_exec(
            *cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            cwd=self.mount,
        )

    async def subscribe_with_states(self, states) -> asyncio.subprocess.Process:
        self.mkdir(".edenfs-notifications-state")
        for state in states:
            self.mkdir(f".edenfs-notifications-state/{state}")
        # Wait to prevent the file creation from showing up in the stream
        time.sleep(1)
        args = [
            "notify",
            "changes-since",
            "--subscribe",
            "--throttle",
            "5",
            "--json",
            self.mount,
        ]
        for state in states:
            args.append("--deferred-states")
            args.append(state)
        cmd, env = self.eden.get_edenfsctl_cmd_env(*args)
        env["EDENFS_LOG"] = "edenfs=trace"

        return await asyncio.create_subprocess_exec(
            *cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            cwd=self.mount,
        )

    async def enter_state(self, state, duration=30) -> asyncio.subprocess.Process:
        # set default duration to prevent accidental spinlocks
        args = [
            "notify",
            "enter-state",
            state,
            self.mount,
        ]
        if duration:
            args.append("--duration")
            args.append(str(duration))
        cmd, env = self.eden.get_edenfsctl_cmd_env(*args)
        env["EDENFS_LOG"] = "edenfs=trace"

        return await asyncio.create_subprocess_exec(
            *cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=self.mount,
        )

    async def get_states(self) -> asyncio.subprocess.Process:
        args = [
            "notify",
            "get-states",
            self.mount,
            "--json",
        ]
        cmd, env = self.eden.get_edenfsctl_cmd_env(*args)
        env["EDENFS_LOG"] = "edenfs=trace"

        return await asyncio.create_subprocess_exec(
            *cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=self.mount,
        )

    async def next_event(
        self, sub: asyncio.subprocess.Process, timeout: int = 5
    ) -> t.Optional[t.Dict[str, t.Any]]:
        """Wait for the next event generated from the subscription.

        We need asyncio here so we can set a timeout. Otherwise we will need to
        manually select.
        """
        stdout = sub.stdout
        if not stdout:
            raise RuntimeError("no stdout captured")

        try:
            line = await asyncio.wait_for(stdout.readline(), timeout=timeout)
        except asyncio.TimeoutError:
            return None
        try:
            # Newline separates each json event
            await asyncio.wait_for(stdout.readline(), timeout=timeout)
        except asyncio.TimeoutError:
            return None
        if not line or line == b"\n":
            return None
        try:
            return json.loads(line.decode())
        except json.JSONDecodeError:
            print(f"failed to decode: {line}", file=sys.stderr)
            raise

    async def wait_for_next_event(
        self,
        sub: asyncio.subprocess.Process,
        action: t.Callable[[], Any] = lambda: None,
        attempt: int = 10,
    ) -> t.Optional[t.Dict[str, t.Any]]:
        """Pull until the next event generated from the subscription

        When the system is under heavy load or when we are testing dev mode
        binary, the subscription may finish initializing after the write event
        is already done by the test. So we retry a few times to make sure the
        subscription sees it.
        """
        for _ in range(attempt):
            await asyncio.sleep(0.1)
            action()
            event = await self.next_event(sub)
            if event:
                return event
        return None

    async def wait_for_file(self, path: str, timeout: float = 5.0) -> None:
        """Wait for the file to be created in the mount."""
        while True:
            if os.path.exists(os.path.join(self.mount, path)):
                return
            await asyncio.sleep(0.1)
            if timeout > 0:
                timeout -= 0.1
            if timeout <= 0:
                raise RuntimeError(f"Timed out waiting for {path}")

    async def wait_for_state(self, state: str) -> None:
        await self.wait_for_file(
            f".edenfs-notifications-state/{state}/{state}.notify",
            timeout=10.0,
        )

    async def test_debug_subscribe(self) -> None:
        subscription = await self.subscribe()

        event = await self.wait_for_next_event(subscription)

        self.assertIsNotNone(event)
        self.assertIsNotNone(event["to_position"])
        self.assertIsNotNone(event["to_position"]["mount_generation"])
        self.assertIsNotNone(event["to_position"]["sequence_number"])
        self.assertIsNotNone(event["to_position"]["snapshot_hash"])
        self.assertListEqual(event["changes"], [])

        event = await self.wait_for_next_event(
            subscription, lambda: self.write_file("hello", "test")
        )

        self.assertIsNotNone(event)
        self.assertIsNotNone(event["to_position"])
        self.assertIsNotNone(event["to_position"]["mount_generation"])
        self.assertIsNotNone(event["to_position"]["sequence_number"])
        self.assertIsNotNone(event["to_position"]["snapshot_hash"])
        self.assertListEqual(
            event["changes"],
            [
                {
                    "SmallChange": {
                        "Modified": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111],
                        }
                    }
                }
            ],
        )

    async def test_debug_subscribe_with_states(self) -> None:
        subscription = await self.subscribe_with_states(["hello"])
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertListEqual(event["changes"], [])

        self.write_file("hello2", "test")
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertTrue(
            {
                "SmallChange": {
                    "Added": {
                        "file_type": "Regular",
                        "path": [104, 101, 108, 108, 111, 50],
                    }
                }
            }
            in event["changes"],
            msg=f"changes: {event['changes']}",
        )

        state_process = await self.enter_state("hello")
        await self.wait_for_state("hello")
        event = await self.wait_for_next_event(subscription)
        if event and "event_type" not in event:
            # Sometimes the Modified change from the previous change gets chunked
            # into a separate event from Added. Read it and bypass.
            event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Entered", msg=f"event: {event}")
        self.assertEqual(event["state"], "hello", msg=f"event: {event}")

        # expect this to return None since the state is asserted
        self.write_file("hello3", "test")
        time.sleep(1)
        self.rename("hello3", "hello4")
        time.sleep(1)
        if sys.platform == "linux":
            self.rm("hello4")
        time.sleep(1)
        event = await self.wait_for_next_event(subscription, attempt=1)
        self.assertIsNone(event)

        # exit the state
        state_process.terminate()
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
        self.assertEqual(event["state"], "hello", msg=f"event: {event}")

        # Get deferred changes
        changes = []
        while event is not None:
            event = await self.wait_for_next_event(subscription, attempt=1)
            if event is None:
                break
            if "changes" in event:
                changes.extend(event["changes"])
            else:
                # Check that a double state event is the same leave as above
                if "event_type" in event:
                    self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
                    self.assertEqual(event["state"], "hello", msg=f"event: {event}")
        if sys.platform != "win32":
            self.assertTrue(
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 51],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            self.assertTrue(
                {
                    "SmallChange": {
                        "Renamed": {
                            "file_type": "Regular",
                            "from": [104, 101, 108, 108, 111, 51],
                            "to": [104, 101, 108, 108, 111, 52],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            # Rm behavior is different on Linux and Mac
            # Mac will sometimes rename the file to a nfs.date.xxxx and then delete it
            if sys.platform == "linux":
                self.assertTrue(
                    {
                        "SmallChange": {
                            "Removed": {
                                "file_type": "Regular",
                                "path": [104, 101, 108, 108, 111, 52],
                            }
                        }
                    }
                    in changes,
                    msg=f"changes: {changes}",
                )
        else:
            # Windows has no rename, only add/remove
            self.assertTrue(
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 51],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            self.assertTrue(
                {
                    "SmallChange": {
                        "Removed": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 51],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            self.assertTrue(
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 52],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )

    async def test_debug_subscribe_with_state_asserted_before_start(self) -> None:
        # Assert the state first, then start the subscription with it deferred.
        self.mkdir(".edenfs-notifications-state")
        self.mkdir(".edenfs-notifications-state/hello")
        state_process = await self.enter_state("hello")
        await self.wait_for_state("hello")

        subscription = await self.subscribe_with_states(["hello"])
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertListEqual(event["changes"], [])

        added_hello2 = {
            "SmallChange": {
                "Added": {
                    "file_type": "Regular",
                    "path": [104, 101, 108, 108, 111, 50],
                }
            }
        }
        self.write_file("hello2", "test")
        time.sleep(1)

        # Collect everything delivered while the state is still asserted.
        changes_before_left = []
        event = await self.wait_for_next_event(subscription, attempt=1)
        while event is not None:
            self.assertNotIn("event_type", event, msg=f"event: {event}")
            changes_before_left.extend(event["changes"])
            event = await self.wait_for_next_event(subscription, attempt=1)

        # Exit the state; the stream reports it.
        state_process.terminate()
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
        self.assertEqual(event["state"], "hello", msg=f"event: {event}")

        changes_after_left = []
        event = await self.wait_for_next_event(subscription, attempt=1)
        while event is not None:
            if "changes" in event:
                changes_after_left.extend(event["changes"])
            event = await self.wait_for_next_event(subscription, attempt=1)

        # The state was asserted before the stream started, so the change made
        # while it was asserted is deferred until the state is left.
        self.assertNotIn(
            added_hello2, changes_before_left, msg=f"changes: {changes_before_left}"
        )
        self.assertIn(
            added_hello2, changes_after_left, msg=f"changes: {changes_after_left}"
        )

    async def test_debug_subscribe_with_underscore_state(self) -> None:
        # A state whose name contains an underscore is asserted under the lock
        # name "my_state"; the journal reports that name back.
        subscription = await self.subscribe_with_states(["my_state"])
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertListEqual(event["changes"], [])

        state_process = await self.enter_state("my_state")
        await self.wait_for_state("my_state")
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Entered", msg=f"event: {event}")
        self.assertEqual(event["state"], "my_state", msg=f"event: {event}")

        # Kill the state holder; the timer check reports the state as left.
        state_process.terminate()
        await state_process.wait()
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
        self.assertEqual(event["state"], "my_state", msg=f"event: {event}")

    async def test_debug_subscribe_with_multiple_states(self) -> None:
        subscription = await self.subscribe_with_states(["hello", "goodbye"])
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertListEqual(event["changes"], [])

        self.write_file("hello2", "test")
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertTrue(
            {
                "SmallChange": {
                    "Added": {
                        "file_type": "Regular",
                        "path": [104, 101, 108, 108, 111, 50],
                    }
                }
            }
            in event["changes"],
            msg=f"changes: {event['changes']}",
        )

        hello_process = await self.enter_state("hello")
        await self.wait_for_state("hello")
        event = await self.wait_for_next_event(subscription)
        if event and "event_type" not in event:
            # Sometimes the Modified change from the previous change gets chunked
            # into a separate event from Added. Read it and bypass.
            event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Entered", msg=f"event: {event}")
        self.assertEqual(event["state"], "hello", msg=f"event: {event}")

        # expect this to return None since the state is asserted
        self.write_file("hello3", "test")
        time.sleep(1)
        self.rename("hello3", "hello4")
        time.sleep(1)
        if sys.platform == "linux":
            self.rm("hello4")
        time.sleep(1)
        event = await self.wait_for_next_event(subscription, attempt=1)
        self.assertIsNone(event)

        goodbye_process = await self.enter_state("goodbye")
        await self.wait_for_state("goodbye")
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Entered", msg=f"event: {event}")
        self.assertEqual(event["state"], "goodbye", msg=f"event: {event}")

        # exit the first state
        hello_process.terminate()
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
        self.assertEqual(event["state"], "hello", msg=f"event: {event}")

        # expect this to return None since the second state is asserted
        event = await self.wait_for_next_event(subscription, attempt=1)
        try:
            self.assertIsNone(event)
        except AssertionError:
            # Check for double state leave
            if event and "event_type" in event and event["event_type"] == "Left":
                self.assertEqual(event["state"], "hello", msg=f"event: {event}")
                event = await self.wait_for_next_event(subscription, attempt=1)
                self.assertIsNone(event)
            else:
                raise

        # exit the second state
        goodbye_process.terminate()
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
        self.assertEqual(event["state"], "goodbye", msg=f"event: {event}")

        # Get deferred changes
        changes = []
        while event is not None:
            event = await self.wait_for_next_event(subscription, attempt=1)
            if event is None:
                break
            if "changes" in event:
                changes.extend(event["changes"])
            else:
                # Check that a double state event is the same leave as above
                if "event_type" in event:
                    self.assertEqual(event["event_type"], "Left", msg=f"event: {event}")
                    self.assertEqual(event["state"], "goodbye", msg=f"event: {event}")
        if sys.platform != "win32":
            self.assertTrue(
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 51],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            self.assertTrue(
                {
                    "SmallChange": {
                        "Renamed": {
                            "file_type": "Regular",
                            "from": [104, 101, 108, 108, 111, 51],
                            "to": [104, 101, 108, 108, 111, 52],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            # Rm behavior is different on Linux and Mac
            # Mac will sometimes rename the file to a nfs.date.xxxx and then delete it
            if sys.platform == "linux":
                self.assertTrue(
                    {
                        "SmallChange": {
                            "Removed": {
                                "file_type": "Regular",
                                "path": [104, 101, 108, 108, 111, 52],
                            }
                        }
                    }
                    in changes,
                    msg=f"changes: {changes}",
                )
        else:
            # Windows has no rename, only add/remove
            self.assertTrue(
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 51],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            self.assertTrue(
                {
                    "SmallChange": {
                        "Removed": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 51],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )
            self.assertTrue(
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": [104, 101, 108, 108, 111, 52],
                        }
                    }
                }
                in changes,
                msg=f"changes: {changes}",
            )

    async def test_subscribe_unpack_commit_transitions(self) -> None:
        self.eden_repo.write_file("new_file", "hola\n")
        second_commit = self.eden_repo.commit("Add new file.")

        subscription = await self.subscribe("--unpack-commit-transitions")

        event = await self.wait_for_next_event(subscription)

        self.assertIsNotNone(event)
        self.assertIsNotNone(event["to_position"])
        self.assertIsNotNone(event["to_position"]["mount_generation"])
        self.assertIsNotNone(event["to_position"]["sequence_number"])
        self.assertIsNotNone(event["to_position"]["snapshot_hash"])
        self.assertListEqual(event["changes"], [])

        self.eden_repo.update(self.initial_commit)

        event = await self.wait_for_next_event(subscription, attempt=2)

        self.assertIsNotNone(event)
        self.assertIsNotNone(event["to_position"])
        self.assertIsNotNone(event["to_position"]["mount_generation"])
        self.assertIsNotNone(event["to_position"]["sequence_number"])
        self.assertIsNotNone(event["to_position"]["snapshot_hash"])
        self.assertListEqual(
            event["changes"],
            [
                {
                    "LargeChange": {
                        "CommitTransition": {
                            "from": util.hex_str_to_int_array(second_commit),
                            "to": util.hex_str_to_int_array(self.initial_commit),
                        }
                    }
                },
                {
                    "SmallChange": {
                        "Removed": {
                            "file_type": "Regular",
                            "path": util.str_to_int_array("new_file"),
                        }
                    }
                },
            ],
            msg=f"event: {event['changes']}",
        )

        self.eden_repo.update(second_commit)
        event = await self.wait_for_next_event(subscription)

        self.assertIsNotNone(event)
        self.assertIsNotNone(event["to_position"])
        self.assertIsNotNone(event["to_position"]["mount_generation"])
        self.assertIsNotNone(event["to_position"]["sequence_number"])
        self.assertIsNotNone(event["to_position"]["snapshot_hash"])
        self.assertListEqual(
            event["changes"],
            [
                {
                    "LargeChange": {
                        "CommitTransition": {
                            "from": util.hex_str_to_int_array(self.initial_commit),
                            "to": util.hex_str_to_int_array(second_commit),
                        }
                    }
                },
                {
                    "SmallChange": {
                        "Added": {
                            "file_type": "Regular",
                            "path": util.str_to_int_array("new_file"),
                        }
                    }
                },
            ],
            msg=f"event: {event['changes']}",
        )

    async def test_get_states(self) -> None:
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(json.loads(stdout.decode()), [])

        hello_process = await self.enter_state("hello")
        await self.wait_for_file(".edenfs-notifications-state/hello/hello.notify")
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(json.loads(stdout.decode()), ["hello"])

        goodbye_process = await self.enter_state("goodbye")
        await self.wait_for_file(".edenfs-notifications-state/goodbye/goodbye.notify")
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(sorted(json.loads(stdout.decode())), ["goodbye", "hello"])

        hello_process.terminate()
        await hello_process.wait()
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(json.loads(stdout.decode()), ["goodbye"])

        goodbye_process.terminate()
        await goodbye_process.wait()
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(json.loads(stdout.decode()), [])

    async def test_multiple_state_enter(self) -> None:
        # This process should get the state
        lock_process = await self.enter_state("hello")
        await self.wait_for_file(".edenfs-notifications-state/hello/hello.notify")
        # This process should fail to enter the state
        fail_state_process = await self.enter_state("hello")
        stdout, stderr = await fail_state_process.communicate()
        self.assertEqual(stdout.decode(), "")
        self.assertIn("State is already asserted", stderr.decode())

        lock_process.terminate()
        await lock_process.wait()

        # Now that the first process has left the state, the second process should be able to enter
        success_state_process = await self.enter_state("hello", duration=1)
        stdout, stderr = await success_state_process.communicate()
        self.assertIn("Holding state for 1 seconds", stdout.decode())

    async def test_subscribe_cleanup_left_over_state(self) -> None:
        hello_notify_file_path = self.eden_repo.get_path(
            ".edenfs-notifications-state/hello/hello.notify"
        )

        # Test that killing the state process leaves the notify file behind
        # Expect that the notify file is created when the state is entered
        self.assertFalse(os.path.exists(hello_notify_file_path))
        state_process = await self.enter_state("hello")
        await self.wait_for_file(hello_notify_file_path, timeout=10)
        self.assertTrue(
            os.path.exists(hello_notify_file_path),
            msg=f"notify file was not created: {hello_notify_file_path}",
        )
        # Expect reading subscription states to keep the notify file if the state is asserted
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(json.loads(stdout.decode()), ["hello"])
        self.assertTrue(
            os.path.exists(hello_notify_file_path),
            msg=f"notify file was not created: {hello_notify_file_path}",
        )
        # Expect killing the state process to leave the notify file behind
        state_process.kill()
        await state_process.communicate()
        self.assertTrue(os.path.exists(hello_notify_file_path))

        # Expect reading subscription states to clear the notify file if the state is not asserted
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        # Expect no states to be asserted, and the notify file to be deleted
        self.assertEqual(json.loads(stdout.decode()), [])
        self.assertFalse(os.path.exists(hello_notify_file_path))

    async def test_subscribe_max_events(self) -> None:
        """Test that --max-events causes the subscription to exit after N non-empty events."""
        subscription = await self.subscribe("--max-events", "2")

        # Initial result (empty changes) should not count toward max_events
        event = await self.wait_for_next_event(subscription)
        self.assertIsNotNone(event)
        self.assertListEqual(event["changes"], [])

        # First non-empty event (count: 1)
        event = await self.wait_for_next_event(
            subscription, lambda: self.write_file("file1", "content1")
        )
        self.assertIsNotNone(event)
        self.assertTrue(len(event["changes"]) > 0, msg=f"changes: {event['changes']}")

        # Second non-empty event (count: 2) -- should hit max_events
        event = await self.wait_for_next_event(
            subscription, lambda: self.write_file("file2", "content2")
        )
        self.assertIsNotNone(event)
        self.assertTrue(len(event["changes"]) > 0, msg=f"changes: {event['changes']}")

        # The process should exit with code 0 after reaching max_events
        returncode = await asyncio.wait_for(subscription.wait(), timeout=10)
        self.assertEqual(
            returncode,
            0,
            msg="Subscription should exit with code 0 after reaching max_events",
        )

    async def test_subscribe_cleanup_left_over_state_multiple_checks(self) -> None:
        hello_notify_file_path = self.eden_repo.get_path(
            ".edenfs-notifications-state/hello/hello.notify"
        )

        # Test that killing the state process leaves the notify file behind
        # Expect that the notify file is created when the state is entered
        self.assertFalse(os.path.exists(hello_notify_file_path))
        state_process = await self.enter_state("hello")
        await self.wait_for_file(hello_notify_file_path, timeout=10)
        self.assertTrue(
            os.path.exists(hello_notify_file_path),
            msg=f"notify file was not created: {hello_notify_file_path}",
        )
        # Expect reading subscription states to keep the notify file if the state is asserted
        get_states_process = await self.get_states()
        stdout, _stderr = await get_states_process.communicate()
        self.assertEqual(json.loads(stdout.decode()), ["hello"])
        self.assertTrue(
            os.path.exists(hello_notify_file_path),
            msg=f"notify file was not created: {hello_notify_file_path}",
        )
        # Expect killing the state process to leave the notify file behind
        state_process.kill()
        await state_process.communicate()
        self.assertTrue(os.path.exists(hello_notify_file_path))

        # Expect reading subscription states to clear the notify file if the state is not asserted
        # Expect that multiple reads will not error
        get_states_process = await self.get_states()
        get_states_process2 = await self.get_states()
        stdout, stderr = await get_states_process.communicate()
        stdout2, stderr2 = await get_states_process2.communicate()
        # Expect no states to be asserted, and the notify file to be deleted
        self.assertEqual(json.loads(stdout.decode()), [])
        self.assertEqual(json.loads(stdout2.decode()), [])
        self.assertEqual(stderr.decode(), "")
        self.assertEqual(stderr2.decode(), "")
        self.assertFalse(os.path.exists(hello_notify_file_path))
