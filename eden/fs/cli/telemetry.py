#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import abc
import getpass
import json
import logging
import os
import platform
import random
import subprocess
import sys
import time
import types
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple, Type, Union

from . import hostname, version

try:
    from eden.fs.cli.facebook.hostcaps import get_fb_info, get_telemetry_user
except ImportError:
    # in OSS define a stub
    def get_fb_info() -> Optional[Dict[str, str]]:
        return None

    def get_telemetry_user() -> str:
        return getpass.getuser()


log: logging.Logger = logging.getLogger(__name__)

_session_id: Optional[int] = None

TelemetryTypes = Union[bool, int, str, float, Set[str], List[str]]


class TelemetrySample(abc.ABC):
    _start_time: float = 0.0
    _success_recorded: bool = False

    def __enter__(self) -> "TelemetrySample":
        self._start_time = time.time()
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        exc_tracebac: Optional[types.TracebackType],
    ) -> bool:
        duration = time.time() - self._start_time
        self.add_double("duration", duration)
        if not self._success_recorded:
            if exc_type is None:
                self.add_bool("success", True)
            else:
                self.add_bool("success", False)
                self.add_string("error", str(exc_value))
        self.log()
        return False

    @abc.abstractmethod
    def add_int(self, name: str, value: int) -> "TelemetrySample":
        raise NotImplementedError()

    @abc.abstractmethod
    def add_string(self, name: str, value: str) -> "TelemetrySample":
        raise NotImplementedError()

    @abc.abstractmethod
    def add_double(self, name: str, value: float) -> "TelemetrySample":
        raise NotImplementedError()

    @abc.abstractmethod
    def add_tags(self, name: str, value: Set[str]) -> "TelemetrySample":
        raise NotImplementedError()

    @abc.abstractmethod
    def add_normvector(self, name: str, value: List[str]) -> "TelemetrySample":
        raise NotImplementedError()

    def add_bool(self, name: str, value: bool) -> "TelemetrySample":
        return self.add_int(name, int(value))

    def add_fields(self, **kwargs: TelemetryTypes) -> "TelemetrySample":
        for name, value in kwargs.items():
            if isinstance(value, bool):
                self.add_bool(name, value)
            elif isinstance(value, str):
                self.add_string(name, value)
            elif isinstance(value, int):
                self.add_int(name, value)
            elif isinstance(value, float):
                self.add_double(name, value)
            elif isinstance(value, set):
                self.add_tags(name, value)
            elif isinstance(value, list):
                self.add_normvector(name, value)
            else:  # unsupported type
                log.error(
                    f"unsupported value type {type(value)} passed to add_fields()"
                )
        return self

    def fail(self, error_msg: str) -> None:
        """Mark the sample as a failure.

        This sets the success field to 0 and sets an error message.

        Prefer using this method over manually setting the "success" field.  Otherwise
        if you log this sample by using it in a `with` context the success field will be
        overwritten with a successful value if the `with` context returns successfully.
        """
        self.add_bool("success", False)
        self.add_string("error", error_msg)
        self._success_recorded = True

    def log(self) -> None:
        """Log the sample to the telemetry data store."""
        self.add_int("time", int(time.time()))
        try:
            self._log_impl()
        except Exception as ex:
            log.warning(f"error logging telemetry sample: {ex}")

    @abc.abstractmethod
    def _log_impl(self) -> None:
        raise NotImplementedError()


class TelemetryLogger(abc.ABC):
    """TelemetryLogger provides an interface for logging structured log events."""

    session_id: int
    user: str
    hostname: str
    os: str
    os_version: str
    eden_version: str

    def __init__(self) -> None:
        self.session_id = get_session_id()
        try:
            self.user = get_telemetry_user()
        except Exception as ex:
            log.warning(f"error determining username for telemetry logging: {ex}")
            self.user = ""
        try:
            self.hostname = hostname.get_normalized_hostname()
        except Exception as ex:
            log.warning(f"error determining hostname for telemetry logging: {ex}")
            self.hostname = ""
        try:
            self.os, self.os_version = get_os_and_ver()
        except Exception as ex:
            log.warning(f"error determining OS information for telemetry logging: {ex}")
            self.os = ""
            self.os_version = ""
        try:
            self.eden_version = version.get_current_version()
        except Exception as ex:
            log.warning(f"error determining EdenFS version for telemetry logging: {ex}")
            self.eden_version = ""

    def new_sample(self, event_type: str, **kwargs: TelemetryTypes) -> TelemetrySample:
        sample = self._create_sample()
        sample.add_string("logged_by", "cli_py")
        sample.add_string("type", event_type)
        sample.add_int("session_id", self.session_id)
        sample.add_string("user", self.user)
        sample.add_string("host", self.hostname)
        sample.add_string("os", self.os)
        sample.add_string("osver", self.os_version)
        sample.add_string("edenver", self.eden_version)

        agent_metadata = os.environ.get("CODING_AGENT_METADATA")
        if agent_metadata:
            for part in agent_metadata.split(","):
                if "=" in part:
                    key, value = part.split("=", 1)
                    if key == "id":
                        sample.add_string("agentic_fingerprint_id", value)
                    elif key == "invocation_id":
                        sample.add_string("agentic_fingerprint_invocation_id", value)

        fb_info = get_fb_info()
        if fb_info:
            for key, value in fb_info.items():
                sample.add_string(key, value)

        sample.add_fields(**kwargs)
        return sample

    def new_empty_sample(self, log_session_id: bool = False) -> TelemetrySample:
        sample = self._create_sample()
        if log_session_id:
            sample.add_int("session_id", self.session_id)
        return sample

    def log(self, event_type: str, **kwargs: TelemetryTypes) -> None:
        self.new_sample(event_type, **kwargs).log()

    @abc.abstractmethod
    def _create_sample(self) -> TelemetrySample:
        raise NotImplementedError()


class JsonTelemetrySample(TelemetrySample):
    def __init__(self, logger: "BaseJsonTelemetryLogger") -> None:
        super().__init__()
        self.ints: Dict[str, int] = {}
        self.strings: Dict[str, str] = {}
        self.doubles: Dict[str, float] = {}
        self.tags: Dict[str, List[str]] = {}
        self.normvectors: Dict[str, List[str]] = {}
        self.logger: "BaseJsonTelemetryLogger" = logger

    def add_int(self, name: str, value: int) -> "JsonTelemetrySample":
        self.ints[name] = value
        return self

    def add_string(self, name: str, value: str) -> "JsonTelemetrySample":
        self.strings[name] = value
        return self

    def add_double(self, name: str, value: float) -> "JsonTelemetrySample":
        self.doubles[name] = value
        return self

    def add_tags(self, name: str, value: Set[str]) -> "JsonTelemetrySample":
        self.tags[name] = list(value)
        return self

    def add_normvector(self, name: str, value: List[str]) -> "JsonTelemetrySample":
        self.normvectors[name] = value
        return self

    def get_json(self) -> str:
        data: Dict[
            str,
            Union[
                Dict[str, str], Dict[str, int], Dict[str, float], Dict[str, List[str]]
            ],
        ] = {}
        data["int"] = self.ints
        data["normal"] = self.strings
        if self.doubles:
            data["double"] = self.doubles
        if self.tags:
            data["tags"] = self.tags
        if self.normvectors:
            data["normvector"] = self.normvectors
        return json.dumps(data)

    def _log_impl(self) -> None:
        self.logger.log_sample(self)


class BaseJsonTelemetryLogger(TelemetryLogger):
    def _create_sample(self) -> TelemetrySample:
        return JsonTelemetrySample(self)

    @abc.abstractmethod
    def log_sample(self, sample: JsonTelemetrySample) -> None:
        raise NotImplementedError()


class ExternalTelemetryLogger(BaseJsonTelemetryLogger):
    """A TelemetryLogger that uses an external process to log samples."""

    cmd: List[str]

    def __init__(self, cmd: List[str]) -> None:
        super().__init__()
        self.cmd = cmd[:]

    def log_sample(self, sample: JsonTelemetrySample) -> None:
        cmd = self.cmd + [sample.get_json()]
        kwargs = {}
        if sys.platform == "win32":
            kwargs = {
                "creationflags": subprocess.CREATE_NO_WINDOW,
                "startupinfo": subprocess.STARTUPINFO(
                    dwFlags=subprocess.STARTF_USESHOWWINDOW
                ),
            }
        try:
            # pyrefly: ignore [bad-argument-type]
            rc = subprocess.call(cmd, **kwargs)
            if rc != 0:
                log.warning(f"telemetry log command returned non-zero exit code {rc}")
        except Exception as ex:
            log.warning(f"error calling telemetry log command: {ex}")


class LocalTelemetryLogger(BaseJsonTelemetryLogger):
    """A TelemetryLogger that logs samples to a local file.
    This is primarily useful just for debugging during development.
    """

    path: Path

    def __init__(self, path: Union[str, Path]) -> None:
        super().__init__()
        self.path = Path(path)

    def log_sample(self, sample: JsonTelemetrySample) -> None:
        with self.path.open("a") as f:
            f.write(sample.get_json() + "\n")


class TestTelemetryLogger(BaseJsonTelemetryLogger):
    """A TelemetryLogger that records all samples in a local list.
    This is primarily useful for use in unit tests, so you can examine the samples
    that were recorded.
    """

    samples: List[JsonTelemetrySample] = []

    def log_sample(self, sample: JsonTelemetrySample) -> None:
        self.samples.append(sample)


class NullTelemetrySample(TelemetrySample):
    def add_int(self, name: str, value: int) -> "NullTelemetrySample":
        return self

    def add_string(self, name: str, value: str) -> "NullTelemetrySample":
        return self

    def add_double(self, name: str, value: float) -> "NullTelemetrySample":
        return self

    def add_tags(self, name: str, value: Set[str]) -> "NullTelemetrySample":
        return self

    def add_normvector(self, name: str, value: List[str]) -> "NullTelemetrySample":
        return self

    def add_bool(self, name: str, value: bool) -> "NullTelemetrySample":
        return self

    def add_fields(self, **kwargs: TelemetryTypes) -> "NullTelemetrySample":
        return self

    def _log_impl(self) -> None:
        pass


class NullTelemetryLogger(TelemetryLogger):
    """A TelemetryLogger that discards all samples."""

    def _create_sample(self) -> TelemetrySample:
        return NullTelemetrySample()


def telemetry_disabled_by_env() -> bool:
    """Return whether environment variables disable Scuba telemetry."""
    # Test harnesses must not be able to opt in to production telemetry.
    if "EDENFS_UNITTEST" in os.environ or "EDENFS_INTEGRATION_TEST" in os.environ:
        return True

    # edenfsctl sets this after applying the Rust build-mode policy.
    if "EDENFS_NO_TELEMETRY" in os.environ:
        return True

    # Developers set this when testing Scuba logging from a dev build.
    if "EDENFS_SCUBA_LOG_FROM_DEV" in os.environ:
        return False

    return False


def get_session_id() -> int:
    global _session_id
    sid = _session_id
    if sid is None:
        sid = random.randrange(2**32)
        _session_id = sid
    return sid


def get_os_and_ver() -> Tuple[str, str]:
    os = platform.system()
    if os == "Darwin":
        os = "macOS"
    if os == "":
        os = "unknown"

    ver = platform.release()
    if ver == "":
        ver = "unknown"

    return os, ver
