#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import binascii
import json
import logging
import os
import subprocess
import sys
import typing
from pathlib import Path
from typing import BinaryIO, Dict, List, Optional, Tuple

import eden.dirstate
from eden.fs.cli.util import (
    create_legacy_filter_id,
    get_environment_suitable_for_subprocess,
    print_stderr,
)

from .config import EdenCheckout

logger: logging.Logger = logging.getLogger(__name__)

_DEFAULT_EXTRA_HGRC_CONTENTS: str = """\
[extensions]
eden =
share =

[ui]
portablefilenames = ignore
"""

# Don't name ".hg" or ".sl" - we don't want the state dir to appear to be a repo itself.
_OFF_MOUNT_REPO_DIR_NAME = "sl-repo-dir"


def off_mount_repo_dir(state_dir: Path) -> Path:
    """Where a checkout using the off-mount layout keeps its real repo dir.

    The in-mount dot dir is only a symlink to this, so this path stays
    readable while the checkout is unmounted.
    """
    return state_dir / _OFF_MOUNT_REPO_DIR_NAME


# Parses the output of `sl config <section>.<key>` where only 0 or 1 value is returned
def parse_config_output(out: str, is_json: bool = True) -> str:
    # Cover case where `sl config` command failed silently
    if out == "":
        return ""

    if is_json:
        json_res = json.loads(out)
        if len(json_res) == 0:
            return ""

        # We expect only 1 result if the config name existed
        assert len(json_res) == 1
        return json_res[0]["value"]
    else:
        return out.replace("\\n", "\n").replace("\\t", "\t")


def get_filter_warning(
    backing_repo_path: Path,
) -> str:
    args = [
        os.environ.get("EDEN_HG_BINARY", "hg"),
        "config",
        "sparse.filter-warning",
        "-Tjson",
    ]

    result = subprocess.run(
        args,
        capture_output=True,
        encoding="utf-8",
        check=False,
        env=get_environment_suitable_for_subprocess(),
        cwd=backing_repo_path,
        timeout=60,
    )

    output = parse_config_output(result.stdout)
    if result.returncode == 0 and output != "":
        output += "\n\n"
    return output


def get_filter_id(
    commit_id: str,
    filter_paths: List[str],
    backing_repo_path: Path,
) -> Optional[bytes]:
    # TODO: Add a helper for determining whether "hg" or "sl" should be used
    args = [
        os.environ.get("EDEN_HG_BINARY", "hg"),
        "debugfilterid",
        "-r",
        commit_id,
    ] + filter_paths

    try:
        result = subprocess.run(
            args,
            capture_output=True,
            check=True,
            env=get_environment_suitable_for_subprocess(),
            cwd=backing_repo_path,
            timeout=60,
        )
        return result.stdout
    except subprocess.CalledProcessError as e:
        # if the command does not exist, we must be running an old version of Sapling. Revert to Legacy filter IDs
        if (
            e.returncode == 255
            and e.stderr is not None
            and b"unknown command" in e.stderr
        ):
            if len(filter_paths) > 1:
                raise Exception(
                    f"'sl debugfilterid' is unavailable, but multiple filter paths were specified: {filter_paths}"
                )
            else:
                print_stderr(
                    "'sl debugfilterid' is unavailable. Falling back to legacy filter creation."
                )
                return create_legacy_filter_id(
                    commit_id, filter_paths[0] if len(filter_paths) == 1 else None
                )

        # The command failed for some other reason. Throw since this is unexpected
        raise Exception(f"Filter generation failed: {e.stderr}")


def _setup_off_mount_hg_dir(checkout_hg_dir: Path, real_hg_dir: Path) -> None:
    try:
        real_hg_dir.mkdir()
    except FileExistsError:
        if not real_hg_dir.is_dir() or real_hg_dir.is_symlink():
            raise Exception(f"{real_hg_dir} exists but is not a directory")
        logger.info("Reusing existing Mercurial state directory %s", real_hg_dir)

    try:
        os.symlink(real_hg_dir, checkout_hg_dir, target_is_directory=True)
    except FileExistsError:
        if not checkout_hg_dir.is_symlink():
            raise Exception(f"{checkout_hg_dir} exists but is not a symlink")

        existing_target = Path(os.readlink(checkout_hg_dir))
        if not existing_target.is_absolute():
            existing_target = checkout_hg_dir.parent / existing_target
        if os.path.abspath(existing_target) != os.path.abspath(real_hg_dir):
            raise Exception(
                f"{checkout_hg_dir} points to {existing_target}, expected {real_hg_dir}"
            )
        logger.info(
            "Reusing existing Mercurial state symlink %s -> %s",
            checkout_hg_dir,
            real_hg_dir,
        )


def setup_hg_dir(
    checkout: EdenCheckout, commit_id: str, filter_paths: Optional[List[str]] = None
) -> None:
    config = checkout.get_config()

    checkout_hg_dir = checkout.hg_dot_path
    real_hg_dir = checkout_hg_dir
    if config.off_mount_repo_dir:
        real_hg_dir = off_mount_repo_dir(checkout.state_dir)

    if config.off_mount_repo_dir:
        _setup_off_mount_hg_dir(checkout_hg_dir, real_hg_dir)
    else:
        try:
            real_hg_dir.mkdir()
        except FileExistsError:
            raise Exception(f"{real_hg_dir} directory already exists")

    # repo config file (hgrc for .hg, config for .sl)
    hgrc_data = get_hgrc_data(checkout)
    config_name = config_file_for_dot_dir(checkout_hg_dir.name)
    (checkout_hg_dir / config_name).write_text(hgrc_data)

    # requires
    requires_data = get_requires_data(checkout)
    (checkout_hg_dir / "requires").write_text(requires_data)

    # Create the shared and sharedpath files, which tell mercurial where it should
    # really look for most of the mercurial state, and that bookmarks should also be
    # shared.
    #
    # Note that the sharedpath file intentionally does not include a trailing newline.
    # This matches how it is generated by mercurial.
    backing_hg_dir = get_backing_hg_dir(checkout)
    (checkout_hg_dir / "sharedpath").write_bytes(bytes(backing_hg_dir))
    (checkout_hg_dir / "shared").write_text("bookmarks\n")

    # Create an empty bookmarks file
    (checkout_hg_dir / "bookmarks").touch()

    # Create a branch file with the contents "default\n". Even though we do not
    # use branches, we have seen some users with a function in their .bashrc
    # that categorically reads the .hg/branch file to include in their prompt.
    (checkout_hg_dir / "branch").write_text("default\n")

    # Write the parents to the dirstate file.
    with typing.cast(BinaryIO, (checkout_hg_dir / "dirstate").open("wb")) as f:
        parents = (binascii.unhexlify(commit_id), b"\x00" * 20)
        tuples_dict: Dict[str, Tuple[str, int, int]] = {}
        copymap: Dict[str, str] = {}
        eden.dirstate.write(f, parents, tuples_dict, copymap)

    # If the checkout is using FilteredFS, we need to write an initial
    # .hg/sparse file that indicates no filter is active.
    if checkout.get_config().scm_type == "filteredhg":
        filter_warning = get_filter_warning(checkout.get_backing_repo_path())
        filters = "".join([f"%include {f}\n" for f in sorted(filter_paths or [])])
        filter_content = (filter_warning + filters) if filter_paths is not None else ""
        (checkout_hg_dir / "sparse").write_text(filter_content)


def get_backing_hg_dir(checkout: EdenCheckout) -> Path:
    """Given an EdenCheckout object, return the path to the actual .hg/ directory that
    contains the mercurial data store.

    This is the path that .hg/sharedpath should point to.
    """
    backing_repo = checkout.get_config().backing_repo
    return backing_repo / sniff_dot_dir(backing_repo)


def get_hgrc_data(checkout: EdenCheckout) -> str:
    backing_hg_dir = get_backing_hg_dir(checkout)
    extra_hgrc = checkout.instance.get_config_value(
        "hg.extra_hgrc", default=_DEFAULT_EXTRA_HGRC_CONTENTS
    )

    if extra_hgrc and extra_hgrc[-1] != "\n":
        extra_hgrc += "\n"

    config_name = config_file_for_dot_dir(backing_hg_dir.name)
    try:
        orig_hgrc = (backing_hg_dir / config_name).read_text()
    except FileNotFoundError:
        # Repositories aren't required to have a config file.
        # Make sure the .hg/.sl directory itself exists though, to confirm
        # we weren't called with incorrect arguments.
        if not backing_hg_dir.is_dir():
            raise Exception(f"backing repository does not exist: {backing_hg_dir}")
        orig_hgrc = ""

    return orig_hgrc + "\n" + extra_hgrc


def get_requires_data(checkout: EdenCheckout) -> str:
    backing_hg_dir = get_backing_hg_dir(checkout)
    try:
        orig_requires_data = (backing_hg_dir / "requires").read_text()
        requires = set(orig_requires_data.splitlines())
    except FileNotFoundError:
        requires = set()

    # Add eden as a requirement
    requires.add("eden")

    # Drop other dirstate-related requirements that are specific to
    # the backing repository's dirstate.
    requires.discard("sqldirstate")
    requires.discard("treedirstate")
    requires.discard("windowssymlinks")
    requires.discard("edensparse")

    if sys.platform == "win32":
        requires.add("windowssymlinks")

    if checkout.get_config().scm_type == "filteredhg":
        requires.add("edensparse")

    return "\n".join(sorted(requires)) + "\n"


_possible_dot_dirs = (".hg", ".sl")

# Map from dot dir name to the config file name inside it.
_dot_dir_config_file = {".hg": "hgrc", ".sl": "config"}


def config_file_for_dot_dir(dot_dir: str) -> str:
    return _dot_dir_config_file.get(dot_dir, "hgrc")


def sniff_dot_dir(repo_root: Path) -> str:
    for dot_dir in _possible_dot_dirs:
        if (repo_root / dot_dir).exists():
            return dot_dir

    # SL_REPO_IDENTITY/HGREPO_IDENTITY specifically controls the repo format
    # (dot dir flavor), so check it first.
    repo_ident = os.environ.get("SL_REPO_IDENTITY") or os.environ.get("HGREPO_IDENTITY")
    if repo_ident in {"hg", "sl"}:
        return "." + repo_ident

    env_ident = os.environ.get("HGIDENTITY") or os.environ.get("SL_IDENTITY")
    if env_ident in {"hg", "sl"}:
        return "." + env_ident

    return _possible_dot_dirs[0]
