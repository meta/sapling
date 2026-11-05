#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import datetime
import errno
import os
from typing import AnyStr, Dict, List, Optional


class Repository:
    def __init__(self, path: str) -> None:
        self.path = path

        # Default author and timestamp info for commits
        self.author_name = "A. Person"
        self.author_email = "person@example.com"
        self.commit_time = datetime.datetime(year=2000, month=1, day=1)
        self.commit_time_delta = datetime.timedelta(seconds=1)

    def get_commit_time(self) -> datetime.datetime:
        """
        Get a datetime object to use for the next commit.

        Rather than using real wall clock time, we use an internally maintained
        date to ensure that we get the same commit hashes across repeated test
        runs.

        The date is advanced for each commit made.
        """
        current = self.commit_time
        self.commit_time += self.commit_time_delta
        return current

    def init(self) -> None:
        raise NotImplementedError("subclasses must implement init()")

    def get_type(self) -> str:
        """Returns the type of this repo as a string, e.g. "hg"."""
        raise NotImplementedError("subclasses must implement get_type()")

    def get_head_hash(self) -> str:
        """Returns the 40-character hex hash for HEAD."""
        raise NotImplementedError("subclasses must implement get_head_hash()")

    def commit(
        self,
        message: str,
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
        date: Optional[datetime.datetime] = None,
        amend: bool = False,
    ) -> str:
        """
        Create a commit.
        Returns the new commit hash as a 40-character hexadecimal string.
        """
        raise NotImplementedError("subclasses must implement commit()")

    def add_file(self, path: str) -> None:
        self.add_files([path])

    def add_files(self, paths: List[str]) -> None:
        raise NotImplementedError("subclasses must implement add_files()")

    def remove_file(self, path: str) -> None:
        self.remove_files([path])

    def remove_files(self, paths: List[str], force: bool = False) -> None:
        raise NotImplementedError("subclasses must implement remove_files()")

    def get_path(self, *args: str) -> str:
        for arg in args:
            assert not os.path.isabs(arg), "must not be absolute: %r" % (arg,)
        return os.path.join(self.path, *args)

    def get_canonical_root(self) -> str:
        """Returns cwd to use when calling scm commands."""
        raise NotImplementedError("subclasses must implement get_canonical_root()")

    def mkdir(self, path: str) -> None:
        full_path = self.get_path(path)
        try:
            os.makedirs(full_path)
        except OSError as ex:
            if ex.errno != errno.EEXIST:
                raise

    def make_parent_dir(self, path: str) -> None:
        dirname = os.path.dirname(path)
        if dirname:
            self.mkdir(dirname)

    def write_file(
        self, path: str, contents: AnyStr, mode: Optional[int] = None, add: bool = True
    ) -> None:
        """
        Create or overwrite a file with the given contents.
        """
        self.make_parent_dir(path)

        if mode is None:
            mode = 0o644

        full_path = self.get_path(path)
        if isinstance(contents, str):
            # pyrefly: ignore [bad-assignment]
            contents = contents.encode()

        with open(full_path, "wb") as f:
            # pyre-fixme[6]: For 1st argument expected `Buffer` but got `AnyStr`.
            f.write(contents)

        os.chmod(full_path, mode)

        if add:
            self.add_file(path)

    def symlink(
        self,
        path: str,
        contents: str,
        add: bool = True,
        target_is_directory: bool = False,
    ) -> None:
        """
        Create a symlink at the specified path, pointed at the given
        destination path contents.
        """
        self.make_parent_dir(path)
        full_path = self.get_path(path)
        try:
            os.unlink(full_path)
        except OSError as ex:
            if ex.errno != errno.ENOENT:
                raise

        if os.name == "nt":
            contents = contents.replace("/", "\\")
        os.symlink(contents, full_path, target_is_directory=target_is_directory)
        if add:
            self.add_file(path)

    def update(self, rev: str, clean: bool = False, merge: bool = False) -> str:
        raise NotImplementedError()

    def push(self, rev: str, target: str, create: bool = False) -> str:
        raise NotImplementedError()

    def run(
        self, *args: str, encoding: str = "utf-8", env: Optional[Dict[str, str]] = None
    ) -> str:
        """Invokes command line args."""
        raise NotImplementedError("subclasses must implement run()")
