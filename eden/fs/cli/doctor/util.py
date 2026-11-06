# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import math
import os
import subprocess
import sys
import traceback
from datetime import timedelta
from pathlib import Path
from typing import Dict, List, Optional, Set

from eden.fs.cli.config import EdenCheckout, EdenInstance
from eden.fs.cli.util import get_environment_suitable_for_subprocess
from eden.fs.service.eden.thrift_types import (
    GetStatInfoParams,
    MountInodeInfo,
    MountState,
    STATS_MOUNTS_STATS,
)


class CheckoutInfo:
    def __init__(
        self,
        instance: EdenInstance,
        path: Path,
        backing_repo: Optional[Path] = None,
        running_state_dir: Optional[Path] = None,
        configured_state_dir: Optional[Path] = None,
        state: Optional[MountState] = None,
        mount_inode_info: Optional[MountInodeInfo] = None,
        visible_in_daemon_namespace: Optional[bool] = None,
        config_error: Optional[Exception] = None,
    ) -> None:
        self.instance = instance
        self.path = path
        self.backing_repo = backing_repo
        self.running_state_dir = running_state_dir
        self.configured_state_dir = configured_state_dir
        self.state = state
        self.mount_inode_info = mount_inode_info
        self.visible_in_daemon_namespace = visible_in_daemon_namespace
        # Set when the checkout's config.toml could not be read, in which case
        # backing_repo is unknown and stays None.
        self.config_error = config_error

    def get_checkout(self) -> EdenCheckout:
        state_dir = (
            self.running_state_dir
            if self.running_state_dir is not None
            else self.configured_state_dir
        )
        assert state_dir is not None
        return EdenCheckout(self.instance, self.path, state_dir)

    def get_backing_repo(self) -> Path:
        # Though the backing repo is optional, this is really just so that
        # we can create the CheckoutInfo without one and assign it later
        # so the backing repo should have been set to a non None value.
        assert self.backing_repo is not None
        return self.backing_repo


def get_dependent_repos(
    backing_repo: Path, all_repos: List[CheckoutInfo]
) -> List[Path]:
    dependent_repos = []
    for repo in all_repos:
        # Checkouts with an unreadable config have no backing repo.
        if repo.backing_repo == backing_repo:
            dependent_repos.append(repo.path)
    return dependent_repos


# Runs `hg doctor` in a backing repo `backing_repo`.
# `dependent_repos` are the EdenFS repos that use this backing repo. These
# are used for display purposes. Note that if clones have failed the set of
# dependent repos may be empty.
# `checked_backing_repos` is a set of backing repos in which we have already run
# `hg doctor`. We keep this around so that we don't attempt to run hg doctor
# multiple times in the same backing repo. Running it multiple times will
# not fix any extra errors and `hg doctor` can be pretty slow, so we save time
# this way.
def hg_doctor_in_backing_repo(
    backing_repo: Path, dependent_repos: List[Path], checked_backing_repos: Set[str]
) -> Optional[str]:
    # We use a set of strings instead of a set of paths because paths have
    # weird equivalence. behavior. Paths that str to the same string do not
    # report as equal.
    if str(backing_repo) in checked_backing_repos:
        return
    checked_backing_repos.add(str(backing_repo))

    hg = os.environ.get("EDEN_HG_BINARY", "hg")

    env = get_environment_suitable_for_subprocess()
    env["HG_DOCTOR_SKIP_EDEN_DOCTOR"] = "1"

    # If hg doctor finds a problem, it typically prints some human readable
    # text to stderr (whether it successfully fixes the issue or not).
    # It would be far better if we could get some structured output from hg
    # doctor that would allow us to indicate better if problems were found and
    # if problems were fixed.  T107874185
    result = subprocess.run(
        [hg, "doctor", "--noninteractive"],
        env=env,
        capture_output=True,
        cwd=backing_repo,
    )

    exitcode = result.returncode
    formatted_out = result.stdout.decode("utf-8")
    formatted_err = result.stderr.decode("utf-8")
    formatted_repos = ", ".join([str(repo) for repo in dependent_repos])

    if len(dependent_repos) == 0:
        recommended_remediation = f"""\
Clones using this repo may fail. Remove
{backing_repo}
to remediate."""
    else:
        recommended_remediation = f"""\
Your EdenFS repo(s) using this backing repo
{formatted_repos}
may be corrupted beyond repair. You can try recloning the effected repo(s)
with `fbclone <repo_type> --eden --reclone` in the parent directory of each of
the repo(s). Alternatively, you can try removing each repo with
`eden rm reponame`, then removing the backing repo (remove as you would a
normal directory not with `eden rm`), and finally running fbclone as normal."""

    # hg doctor failed then things are very bad, just reclone
    if exitcode != 0:
        raise Exception(
            f"""\
`sl doctor` in the backing repository {backing_repo}
failed with exit code {exitcode}. This indicates
{recommended_remediation}

`sl doctor` stdout:
{formatted_out}
`sl doctor` stderr:
{formatted_err}
"""
        )

    # hg doctor prints to stderr whenever it attempts to fix anything.
    # It is hard to determine if hg doctor was actually successful in
    # fixing the issue. So we will just forward this to the user.
    if formatted_err:
        return f"""\
`sl doctor` attempted to fix something in the backing repo
{backing_repo}.
It may or may not have succeeded. If it does not seem to have fixed things, then
may be corrupted beyond repair and {recommended_remediation}

`sl doctor` stdout:
{formatted_out}
`sl doctor` stderr:
{formatted_err}

"""

    return None


def format_traceback(ex: BaseException, indent: str = "") -> List[str]:
    lines = "".join(
        traceback.format_exception(type(ex), ex, ex.__traceback__, chain=False)
    ).splitlines()
    return [indent + line for line in lines]


def format_exception(ex: BaseException, with_tb: bool = False) -> str:
    result = []
    result.append(f"{type(ex).__name__}: {ex}")
    if with_tb:
        result.extend(format_traceback(ex, "│ "))
    # Get CalledProcess output
    if type(ex) is subprocess.CalledProcessError:
        result.append("stdout:")
        result.append(ex.stdout.decode("utf-8"))
        result.append("stderr:")
        result.append(ex.stderr.decode("utf-8"))

    context = ex.__context__

    if context:
        result.append("")
        result.append("Caused by:")

    count = 0
    while context:
        result.append(f"  {count}: {type(context).__name__}: {context}")
        if with_tb:
            result.extend(format_traceback(context, "   │ "))
        context = context.__context__
        count += 1

    return "\n".join(result)


def format_approx_duration(duration: timedelta) -> str:
    """Formats a duration as an approximate, human-readable string."""

    seconds = math.floor(duration.total_seconds())
    if seconds < 0:
        raise ValueError("Unable to format a negative duration")

    units = [("day", 86400), ("hour", 3600), ("minute", 60), ("second", 1)]
    for unit_name, unit_seconds in units:
        if seconds >= unit_seconds:
            count = seconds // unit_seconds
            suffix = "s" if count > 1 else ""
            return f"{count} {unit_name}{suffix}"

    return "a moment"


def get_mount_inode_info(checkout_info: CheckoutInfo) -> Optional[MountInodeInfo]:
    """
    Gets current MountInodeInfo from an EdenInstance and CheckoutInfo.
    """
    instance = checkout_info.instance
    with instance.get_thrift_client() as client:
        internal_stats = client.getStatInfo(
            GetStatInfoParams(statsMask=STATS_MOUNTS_STATS)
        )
        mount_point_info = internal_stats.mountPointInfo or {}
        return mount_point_info.get(bytes(checkout_info.path))
    return None


def get_checkouts_info(instance: EdenInstance) -> Dict[Path, CheckoutInfo]:
    checkouts: Dict[Path, CheckoutInfo] = {}
    # Get information about the checkouts currently known to the running
    # edenfs process
    try:
        with instance.get_thrift_client() as client:
            internal_stats = client.getStatInfo(
                GetStatInfoParams(statsMask=STATS_MOUNTS_STATS)
            )
            mount_point_info = internal_stats.mountPointInfo or {}

            for mount in client.listMounts():
                path = Path(os.fsdecode(mount.mountPoint))
                checkout = CheckoutInfo(
                    instance,
                    path,
                    backing_repo=(
                        Path(os.fsdecode(mount.backingRepoPath))
                        if mount.backingRepoPath is not None
                        else None
                    ),
                    running_state_dir=Path(os.fsdecode(mount.edenClientPath)),
                    state=mount.state,
                    mount_inode_info=mount_point_info.get(mount.mountPoint),
                    visible_in_daemon_namespace=mount.visibleInDaemonNamespace,
                )
                checkouts[path] = checkout

        # Get information about the checkouts listed in the config file
        for configured_checkout in instance.get_checkouts():
            checkout_info = checkouts.get(configured_checkout.path, None)
            if checkout_info is None:
                checkout_info = CheckoutInfo(instance, configured_checkout.path)
                checkouts[checkout_info.path] = checkout_info

            # Set before the config is read: a listed checkout whose config
            # cannot be read is still listed, and must not be reported as
            # unregistered.
            checkout_info.configured_state_dir = configured_checkout.state_dir
            if checkout_info.backing_repo is None:
                try:
                    checkout_info.backing_repo = (
                        configured_checkout.get_config().backing_repo
                    )
                except Exception as ex:
                    # Config file is missing or invalid, so the backing repo
                    # is unknown. Keep the checkout so callers can report it.
                    checkout_info.config_error = ex
        return checkouts
    except Exception as ex:
        print(f"An error occurred while getting checkouts info: {ex}", file=sys.stderr)
        return {}
