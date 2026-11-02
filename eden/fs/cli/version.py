#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import subprocess
from dataclasses import dataclass, field
from datetime import datetime
from typing import Optional, Tuple


# We can live with version dates being a bit old relative to what dnf info shows for build since we're just working with differences
# and strings shown to users.


@dataclass
class VersionInfo:
    running_version: Optional[str] = field(default=None)
    running_version_age: Optional[int] = field(default=None)
    installed_version: Optional[str] = field(default=None)
    installed_version_age: Optional[int] = field(default=None)
    ages_deltas: Optional[int] = field(default=None)
    is_eden_running: bool = field(default=False)
    is_dev: bool = field(default=False)


def get_version_info(
    running_version: Optional[str],
) -> VersionInfo:
    is_eden_running = running_version is not None
    is_dev = False
    installed_version = get_current_version()
    installed_version_age: Optional[int] = None
    installed_version_datetime = date_from_version(installed_version)
    if installed_version_datetime:
        # Prevent edge case where age is negative due to timezone differences
        installed_version_age = max(
            (datetime.now() - installed_version_datetime).days, 0
        )

    running_version_datetime = None
    running_version_age: Optional[int] = None

    running_version_datetime = date_from_version(running_version)
    if running_version_datetime:
        # Prevent edge case where age is negative due to timezone differences
        running_version_age = max((datetime.now() - running_version_datetime).days, 0)

    ages_deltas: Optional[int] = (
        (running_version_age - installed_version_age)
        if running_version_age is not None
        and installed_version_age is not None
        and running_version_age != installed_version_age
        else None
    )

    if running_version is not None and (
        running_version.startswith("-") or running_version.endswith("-")
    ):
        is_dev = True

    return VersionInfo(
        running_version,
        running_version_age,
        installed_version,
        installed_version_age,
        ages_deltas,
        is_eden_running,
        is_dev,
    )


# returns (installed_version, release) tuple
def get_installed_eden_rpm_version_parts() -> Optional[Tuple[str, str]]:
    fields = ("version", "release")
    query_fmt = r"\n---\n".join(f"%{{{f}}}" for f in fields)
    cmd = ["rpm", "-q", "fb-eden", "--queryformat", query_fmt]
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, encoding="utf-8")
    if proc.returncode != 0:
        return None
    parts = proc.stdout.split("\n---\n")
    assert len(parts) == 2, f"unexpected output: {proc.stdout!r}"
    return (parts[0], parts[1])


def format_eden_version(parts: Optional[Tuple[str, str]]) -> str:
    if parts is None:
        return "<Not Installed>"
    version = parts[0] or ""
    release = parts[1] or ""
    return f"{version}-{release}"


def get_installed_eden_rpm_version() -> str:
    return format_eden_version(get_installed_eden_rpm_version_parts())


def get_current_version_parts() -> Tuple[str, str]:
    """Get a tuple containing (version, release) of the currently running code.

    The version and release strings will both be the empty string if this code is part
    of a development build that is not a released package.
    """
    import eden.config

    return (eden.config.VERSION, eden.config.RELEASE)


def get_current_version() -> str:
    """Get a human-readable string describing the version of the currently running code.

    Returns "-" when running a development build that is not part of an official
    versioned release.
    """
    return format_eden_version(get_current_version_parts())


def date_from_version(version: Optional[str]) -> Optional[datetime]:
    """Convert a version string to a datetime object so we can calculate age and deltas, but return None if there's any problem"""
    if version is None or len(version) < 8:
        return None

    try:
        date = datetime.strptime(version[:8], "%Y%m%d")
        return date
    except ValueError:
        return None
