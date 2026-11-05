#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""
These utilities are only expected to work if `sys.argv[0]` is an executable
being run in buck-out.
"""

# pyre-strict

import logging
import os
import shutil
import sys
from typing import Any, Callable, Dict, Generic, List, Optional, Tuple, Type, TypeVar

import eden.config

T = TypeVar("T")

SENTINEL = object()


class cached_property(Generic[T], object):
    def __init__(self, find: Callable[["FindExeClass"], T]) -> None:
        self.name: Optional[str] = None
        self.find = find

    def __get__(self, instance: "FindExeClass", owner: Type["FindExeClass"]) -> T:
        name = self.name
        assert name is not None
        result = instance._cache.get(name, SENTINEL)
        if result is SENTINEL:
            result = self.find(instance)
            instance._cache[name] = result
        return result

    def __set_name__(self, owner: Type["FindExeClass"], name: str) -> None:
        self.name = name


class FindExeClass:
    _BUCK_OUT: Optional[str] = None
    _EDEN_SRC_ROOT: Optional[str] = None
    _TEST_ROOT: str = os.getcwd()

    def __init__(self) -> None:
        self._cache: Dict[str, Any] = {}

    def is_buck_build(self) -> bool:
        return eden.config.BUILD_FLAVOR == "Buck"

    @property
    def BUCK_OUT(self) -> str:
        buck_out = self._BUCK_OUT
        if buck_out is None:
            if not self.is_buck_build():
                raise Exception("There is no buck-out path in a non-Buck build")
            repo_root, buck_out = self._find_repo_root_and_buck_out()
            assert self._BUCK_OUT is not None
        return buck_out

    @property
    def EDEN_SRC_ROOT(self) -> str:
        src_root = self._EDEN_SRC_ROOT
        if src_root is None:
            if self.is_buck_build():
                src_root, buck_out = self._find_repo_root_and_buck_out()
                assert self._EDEN_SRC_ROOT is not None
            else:
                src_root = self._find_cmake_src_dir()
                self._EDEN_SRC_ROOT = src_root
        return src_root

    def get_edenfsctl_env(self) -> Tuple[str, Dict[str, str]]:
        """Get path to edenfsctl and environmental variable required to run it."""
        env = os.environ.copy()
        for name in ("EDEN_HG_BINARY", "HG_REAL_BIN", "HG_ETC_MERCURIAL"):
            path = env.get(name)
            if path:
                env[name] = os.path.abspath(path)
        edenfsctlreal = f"{self._EDENFSCTL_REAL}"
        if sys.platform == "win32":
            # On Windows, par files (which edenfsctl.real is) aren't directly
            # executable and we need to call them with python.
            edenfsctlreal = f"{sys.executable} {edenfsctlreal}"
        env["EDENFSCTL_REAL"] = edenfsctlreal
        return self._EDENFSCTL, env

    @cached_property
    def _EDENFSCTL_REAL(self) -> str:
        return self._find_exe(
            "edenfsctl (Python)",
            env="EDENFSCTL_REAL_PATH",
            buck_path="eden/fs/cli/edenfsctl.par",
            cmake_path="eden/fs/cli/edenfsctl.real/__main__.py",
            system_candidates=["/usr/local/bin/edenfsctl.real"],
        )

    @cached_property
    def _EDENFSCTL(self) -> str:
        return self._find_exe(
            "edenfsctl (Rust)",
            env="EDENFSCTL_RUST_PATH",
            buck_path="eden/fs/cli_rs/edenfsctl/edenfsctl#binary/edenfsctl",
            cmake_path="eden/fs/cli_rs/edenfsctl/release/edenfsctl",
            system_candidates=["/usr/local/bin/edenfsctl"],
        )

    @cached_property
    def EDEN_DAEMON(self) -> str:
        edenfs_suffix = os.environ.get("EDENFS_SUFFIX", "")
        return self._find_exe(
            "edenfs daemon",
            env="EDENFS_SERVER_PATH",
            buck_path="eden/fs/service/edenfs" + edenfs_suffix,
            cmake_path="eden/fs/edenfs",
            system_candidates=["/usr/local/libexec/eden/edenfs"],
        )

    @cached_property
    def EDEN_PRIVHELPER(self) -> Optional[str]:
        return self._find_exe_optional(
            "edenfs privhelper",
            env="EDENFS_PRIVHELPER_PATH",
            buck_path="eden/fs/service/edenfs_privhelper",
            cmake_path="eden/fs/edenfs_privhelper",
            require_found=False,
            system_candidates=["/usr/local/libexec/eden/edenfs_privhelper"],
        )

    @cached_property
    def FSATTR(self) -> str:
        return self._find_exe(
            "fsattr",
            env="EDENFS_FSATTR_BIN",
            buck_path="eden/integration/helpers/fsattr",
        )

    @cached_property
    def FAKE_EDENFS(self) -> str:
        return self._find_exe(
            "fake_edenfs",
            env="EDENFS_FAKE_EDENFS",
            buck_path="eden/integration/helpers/fake_edenfs",
        )

    @cached_property
    def TAKEOVER_TOOL(self) -> str:
        return self._find_exe(
            "takeover_tool",
            env="EDENFS_TAKEOVER_TOOL",
            buck_path="eden/integration/helpers/takeover_tool",
        )

    @cached_property
    def BLAKE3_SUM(self) -> str:
        return self._find_exe(
            "blake3_sum",
            env="BLAKE3_SUM",
            buck_path="eden/integration/helpers/blake3_sum",
        )

    @cached_property
    def CHECK_WINDOWS_RENAME(self) -> str:
        return self._find_exe(
            "check_windows_rename",
            env="EDENFS_CHECK_WINDOWS_RENAME",
            buck_path="eden/integration/helpers/check_windows_rename",
        )

    @cached_property
    def READ_REPARSE_BUFFER(self) -> str:
        return self._find_exe(
            "read_reparse_buffer",
            env="EDENFS_READ_REPARSE_BUFFER",
            buck_path="eden/integration/helpers/read_reparse_buffer",
        )

    @cached_property
    def DROP_PRIVS(self) -> str:
        return self._find_exe(
            "drop_privs",
            env="EDENFS_DROP_PRIVS",
            buck_path="eden/fs/fuse/privhelper/test/drop_privs",
        )

    @cached_property
    def HG(self) -> str:
        hg = self._find_hg()
        logging.info("Found hg binary: %r", hg)
        return hg

    @cached_property
    def HG_REAL(self) -> str:
        hg = self._find_hg_real()
        logging.info("Found hg.real binary: %r", hg)
        return hg

    @cached_property
    def HG_RC_DIR(self) -> str:
        return self._find_src_asset(
            "HG_ETC_MERCURIAL", "fbcode/eden/scm/fb/staticfiles", "etc/mercurial"
        )

    def _find_hg(self) -> str:
        hg_bin = self._find_exe_optional(
            "hg", env="EDEN_HG_BINARY", buck_path="scm/telemetry/hg/hg#binary/hg"
        )
        if hg_bin:
            return hg_bin

        hg_real_bin = shutil.which("hg")
        if hg_real_bin:
            return hg_real_bin

        # Fall back to the hg.real binary
        return self.HG_REAL

    def _find_hg_real(self) -> str:
        hg_real_bin = self._find_exe_optional(
            "hg.real", env="HG_REAL_BIN", buck_path="eden/scm/__hg__/hg.sh"
        )
        if hg_real_bin:
            return hg_real_bin

        hg_real_bin = shutil.which("hg.real")
        if hg_real_bin:
            return hg_real_bin

        hg_real_bin = shutil.which("hg")
        if hg_real_bin:
            return hg_real_bin

        raise Exception("No hg binary found!")

    def _find_exe(
        self,
        name: str,
        env: str,
        buck_path: Optional[str] = None,
        cmake_path: Optional[str] = None,
        buck_cell_path: Optional[str] = None,
        system_candidates: Optional[List[str]] = None,
    ) -> str:
        exe = self._find_exe_optional(
            name=name,
            env=env,
            buck_path=buck_path,
            cmake_path=cmake_path,
            require_found=True,
            buck_cell_path=buck_cell_path,
            system_candidates=system_candidates,
        )
        assert exe is not None
        return exe

    def _find_exe_optional(
        self,
        name: str,
        env: str,
        buck_path: Optional[str] = None,
        cmake_path: Optional[str] = None,
        require_found: bool = False,
        buck_cell_path: Optional[str] = None,
        system_candidates: Optional[List[str]] = None,
    ) -> Optional[str]:
        path = os.environ.get(env) if env is not None else None
        if path:
            path = os.path.abspath(path)
            if not os.access(path, os.X_OK):
                raise Exception(
                    f"able to find {name}: specified as {path!r} "
                    f"by ${env}, but it doesn't exist or we lack"
                    " permissions to execute it"
                )
            return path

        candidates = []
        # In some cases, we want to use a pre-installed (or system) binary
        # instead of a dev built one
        if system_candidates is not None:
            candidates.extend(system_candidates)

        if self.is_buck_build():
            if buck_path is not None:
                buck_out = self.BUCK_OUT
                if buck_cell_path is not None:
                    buck_out = os.path.join(buck_out, buck_cell_path)
                candidates.append(os.path.join(buck_out, "gen", buck_path))
        else:
            # If an explicit CMake output was specified use it,
            # otherwise use the same path as for Buck.
            if cmake_path is not None:
                candidates.append(os.path.join(self._TEST_ROOT, cmake_path))
            elif buck_path is not None:
                candidates.append(os.path.join(self._TEST_ROOT, buck_path))

        if sys.platform == "win32":
            # All of the paths in the candidates list are POSIX-style paths.
            # Convert path separators to Windows-style.
            # Also search for both the path as specified, as well as with a ".exe"
            # suffix, since the CMake build automatically adds a .exe suffix all
            # binaries.
            win_candidates = []
            for path in candidates:
                win_path = path.replace("/", os.path.sep)
                win_candidates.append(win_path)
                win_candidates.append(win_path + ".exe")
            candidates = win_candidates

        for path in candidates:
            if os.access(path, os.X_OK):
                return path

        if require_found:
            raise Exception(f"unable to find {name}: candidates checked={candidates}")

        return None

    def _find_repo_root_and_buck_out(self) -> Tuple[str, str]:
        """Finds the paths to the repo root and the buck-out directory.

        Note that the path to buck-out may not be "buck-out" under the repo
        root because Buck could have been run with `buck --config
        project.buck_out` and sys.argv[0] could be the realpath rather than the
        symlink under buck-out.
        """
        executable = sys.argv[0]
        path = os.path.dirname(os.path.abspath(executable))
        while True:
            parent = os.path.dirname(path)
            parent_basename = os.path.basename(parent)
            if parent_basename == "buck-out":
                src_root = os.path.dirname(parent)
                if os.path.basename(path) in ["bin", "gen"]:
                    buck_out = parent
                else:
                    buck_out = path
                self._EDEN_SRC_ROOT = src_root
                self._BUCK_OUT = buck_out
                return (src_root, buck_out)
            if parent == path:
                raise Exception("Path to repo root not found from %s" % executable)
            path = parent

    def _find_cmake_src_dir(self) -> str:
        src_dir = os.environ.get("CMAKE_SOURCE_DIR", "")
        if src_dir:
            return src_dir

        raise Exception(
            "unable to find source directory: "
            "CMAKE_SOURCE_DIR environment variable is not set"
        )

    def _find_src_asset(self, env: str, src_dir: str, src_name: str) -> str:
        path = os.environ.get(env)
        print(f"{env} = {path}")

        if path:
            path = os.path.join(os.path.abspath(path), src_name)
            if not os.access(path, os.R_OK):
                raise Exception(
                    f"unable to find source asset specified as {path!r} "
                    f"by ${env}, but not available there"
                )
            return path

        return os.path.join(self.EDEN_SRC_ROOT, src_dir, src_name)


# The main FindExe singleton
FindExe = FindExeClass()
