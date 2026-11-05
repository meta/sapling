#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import errno
import json
import os
import stat
import subprocess
import sys
from typing import Set

from .lib import testcase


class BasicTestBase(testcase.EdenRepoTest):
    # pyre-fixme[13]: Attribute `expected_mount_entries` is never initialized.
    expected_mount_entries: Set[str]

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.write_file("adir/file", "foo!\n")
        self.repo.write_file("bdir/test.sh", "#!/bin/bash\necho test\n", mode=0o755)
        self.repo.write_file("bdir/noexec.sh", "#!/bin/bash\necho test\n")
        self.repo.symlink("slink", os.path.join("adir", "file"))

        self.repo.commit("Initial commit.")

        self.expected_mount_entries = {".eden", "adir", "bdir", "hello", "slink"}
        if self.repo.get_type() in ["hg", "filteredhg"]:
            self.expected_mount_entries.add(".hg")


@testcase.eden_repo_test
class BasicTest(BasicTestBase):
    """Exercise some fundamental properties of the filesystem.

    Listing directories, checking stat information, asserting
    that the filesystem is reporting the basic information
    about the sample repo and that it is correct are all
    things that are appropriate to include in this test case.
    """

    def test_version(self) -> None:
        output = self.eden.run_cmd("version", cwd=self.mount)
        lines = output.splitlines()

        # The first line reports info about the edenfsctl version
        cli_info = lines[0]
        self.assertTrue(cli_info.startswith("Installed: "), cli_info)
        cli_version = cli_info[len("Installed: ") :]
        if cli_version == "-":
            # For a dev build the code currently prints "-"
            pass
        else:
            parts = cli_version.split("-")
            self.assertEqual(len(parts[0]), 8)
            self.assertEqual(len(parts[1]), 6)
            self.assertTrue(parts[0].isdigit())
            self.assertTrue(parts[1].isdigit())

        # The second line reports info about the current running edenfs daemon
        running_info = lines[1]
        self.assertTrue(running_info.startswith("Running: "), running_info)
        running_version = running_info[len("Running: ") :].strip()

        # During the integration tests we expect to always be running the same version
        # of edenfsctl and the edenfs daemon.
        self.assertEqual(cli_version, running_version)

    def test_version_json(self) -> None:
        output = self.eden.run_cmd("version", "--json", cwd=self.mount)
        json_out = json.loads(output)
        self.assertTrue("installed" in json_out)
        self.assertTrue("running" in json_out)
        installed_version = json_out["installed"]
        running_version = json_out["running"]

        # During the integration tests we expect to always be running the same
        # version of edenfsctl and the edenfs daemon.
        self.assertEqual(installed_version, running_version)

    def test_file_list(self) -> None:
        self.assert_checkout_root_entries(self.expected_mount_entries)

        adir = os.path.join(self.mount, "adir")
        st = os.lstat(adir)
        self.assertTrue(stat.S_ISDIR(st.st_mode))
        if sys.platform != "win32":
            # No os.getuid() and os.getgid() functions on Windows
            self.assertEqual(st.st_uid, os.getuid())
            self.assertEqual(st.st_gid, os.getgid())

        hello = os.path.join(self.mount, "hello")
        st = os.lstat(hello)
        self.assertTrue(stat.S_ISREG(st.st_mode))

        slink = os.path.join(self.mount, "slink")
        st = os.lstat(slink)
        self.assertTrue(stat.S_ISLNK(st.st_mode))

    def test_symlinks(self) -> None:
        slink = os.path.join(self.mount, "slink")
        self.assertEqual(os.readlink(slink), os.path.join("adir", "file"))

    def test_regular(self) -> None:
        hello = os.path.join(self.mount, "hello")
        with open(hello, "r") as f:
            self.assertEqual("hola\n", f.read())

    def test_dir(self) -> None:
        entries = sorted(os.listdir(os.path.join(self.mount, "adir")))
        self.assertEqual(["file"], entries)

        filename = os.path.join(self.mount, "adir", "file")
        with open(filename, "r") as f:
            self.assertEqual("foo!\n", f.read())

    def test_create(self) -> None:
        filename = self.mount_path / "notinrepo"
        contents = b"created\n"
        filename.write_bytes(contents)

        self.assert_checkout_root_entries(self.expected_mount_entries | {"notinrepo"})

        self.assertEqual(filename.read_bytes(), contents)

        st = os.lstat(filename)
        self.assertEqual(st.st_size, len(contents))
        self.assertTrue(stat.S_ISREG(st.st_mode))

    def test_overwrite(self) -> None:
        hello = self.mount_path / "hello"
        new_contents = b"replaced\n"
        hello.write_bytes(new_contents)

        st = os.lstat(hello)
        self.assertEqual(st.st_size, len(new_contents))

    def test_append(self) -> None:
        hello = self.mount_path / "bdir/test.sh"
        with hello.open("ab") as f:
            f.write("echo more commands\n".encode())

        expected_data = "#!/bin/bash\necho test\necho more commands\n"
        st = os.lstat(hello)
        with hello.open("rb") as f:
            read_back = f.read().decode()
        self.assertEqual(expected_data, read_back)

        expected_len = len(expected_data)
        self.assertEqual(expected_len, st.st_size)

    def test_materialize(self) -> None:
        hello = self.mount_path / "hello"
        # Opening for write should materialize the file with the same
        # contents that we expect
        with hello.open("r+") as f:
            self.assertEqual("hola\n", f.read())

        st = os.lstat(hello)
        self.assertIn(st.st_size, (len(b"hola\n"), len(b"hola\r\n")))

    def test_mkdir(self) -> None:
        # Can't create a directory inside a file that is in the store
        with self.assertRaises(OSError) as context:
            os.mkdir(os.path.join(self.mount, "hello", "world"))

        # Trying to use a file as a directory results in an ENOTDIR error on POSIX
        # systems, but ENOENT on Windows.
        expected_error = errno.ENOTDIR if sys.platform != "win32" else errno.ENOENT
        self.assertEqual(context.exception.errno, expected_error)

        # Can't create a directory when a file of that name already exists
        with self.assertRaises(OSError) as context:
            os.mkdir(os.path.join(self.mount, "hello"))
        self.assertEqual(context.exception.errno, errno.EEXIST)

        # Can't create a directory when a directory of that name already exists
        with self.assertRaises(OSError) as context:
            os.mkdir(os.path.join(self.mount, "adir"))
        self.assertEqual(context.exception.errno, errno.EEXIST)

        buckout = os.path.join(self.mount, "buck-out")
        os.mkdir(buckout)
        st = os.lstat(buckout)
        self.assertTrue(stat.S_ISDIR(st.st_mode))

        self.assert_checkout_root_entries(self.expected_mount_entries | {"buck-out"})

        # Prove that we can recursively build out a directory tree
        deep_name = os.path.join(buckout, "foo", "bar", "baz")
        os.makedirs(deep_name)
        st = os.lstat(deep_name)
        self.assertTrue(stat.S_ISDIR(st.st_mode))

        # And that we can create a file in there too
        deep_file = os.path.join(deep_name, "file")
        with open(deep_file, "w") as f:
            f.write("w00t")
        st = os.lstat(deep_file)
        self.assertTrue(stat.S_ISREG(st.st_mode))

    def test_remove_invalid_paths(self) -> None:
        self.eden.run_unchecked("remove", "/tmp")
        self.eden.run_unchecked("remove", "/root")

    def test_remove_checkout(self) -> None:
        self.assert_checkout_root_entries(self.expected_mount_entries)
        if sys.platform != "win32":
            self.assertTrue(self.eden.in_proc_mounts(self.mount))

        self.eden.remove(self.mount)
        self.eden.wait_for_checkout_removed(self.mount)

        if sys.platform != "win32":
            self.assertFalse(self.eden.in_proc_mounts(self.mount))
        self.assertFalse(os.path.exists(self.mount))

        self.eden.clone(self.repo.path, self.mount)

        self.assert_checkout_root_entries(self.expected_mount_entries)
        if sys.platform != "win32":
            self.assertTrue(self.eden.in_proc_mounts(self.mount))

    def test_hardlink_fails(self) -> None:
        with self.assertRaises(OSError) as context:
            os.link(
                os.path.join(self.mount, "adir", "file"),
                os.path.join(self.mount, "adir", "hardlink"),
            )

            expected_error = errno.EPERM if sys.platform != "win32" else errno.EACCES
            self.assertEqual(context.exception.errno, expected_error)

    if sys.platform == "win32":

        def test_cmd_globbing(self) -> None:
            out = subprocess.check_output(
                "cmd /C dir /B *lo", cwd=self.mount, text=True, stderr=subprocess.STDOUT
            )
            self.assertEqual(out, "hello\n")


@testcase.eden_repo_test
class PosixTest(BasicTestBase):
    """This class contains tests that do not run on Windows.

    This includes things like examining the executable bit in file permissions,
    and Unix-specific calls like mknod() and statvfs()
    """

    def test_mkdir_umask(self) -> None:
        original_umask = os.umask(0o177)
        try:
            dirname = os.path.join(self.mount, "testd1")
            os.mkdir(dirname)
            self.assertEqual(0o600, os.lstat(dirname).st_mode & 0o777)
            dirname = os.path.join(self.mount, "testd2")
            os.umask(0o777)
            os.mkdir(dirname)
            self.assertEqual(0o000, os.lstat(dirname).st_mode & 0o777)
        finally:
            os.umask(original_umask)

    def test_access(self) -> None:
        def check_access(path: str, mode: int) -> bool:
            return os.access(os.path.join(self.mount, path), mode)

        self.assertTrue(check_access("hello", os.R_OK))
        self.assertTrue(check_access("hello", os.W_OK))
        self.assertFalse(check_access("hello", os.X_OK))

        self.assertTrue(check_access("bdir/test.sh", os.R_OK))
        self.assertTrue(check_access("bdir/test.sh", os.W_OK))
        self.assertTrue(check_access("bdir/test.sh", os.X_OK))

        self.assertTrue(check_access("bdir/noexec.sh", os.R_OK))
        self.assertTrue(check_access("bdir/noexec.sh", os.W_OK))
        self.assertFalse(check_access("bdir/noexec.sh", os.X_OK))

        cmd = [os.path.join(self.mount, "bdir/test.sh")]
        out = subprocess.check_output(cmd, stderr=subprocess.STDOUT)
        self.assertEqual(out, b"test\n")

        cmd = [os.path.join(self.mount, "bdir/noexec.sh")]
        with self.assertRaises(OSError) as context:
            out = subprocess.check_output(cmd, stderr=subprocess.STDOUT)
        self.assertEqual(
            errno.EACCES,
            context.exception.errno,
            msg="attempting to run noexec.sh should fail with EACCES",
        )

    def test_create_using_mknod(self) -> None:
        filename = os.path.join(self.mount, "notinrepo")
        os.mknod(filename, stat.S_IFREG | 0o600)
        self.assert_checkout_root_entries(self.expected_mount_entries | {"notinrepo"})

        st = os.lstat(filename)
        self.assertEqual(st.st_size, 0)
        self.assertTrue(stat.S_ISREG(st.st_mode))
        self.assertEqual(st.st_uid, os.getuid())
        self.assertEqual(st.st_gid, os.getgid())
        self.assertEqual(st.st_mode & 0o600, 0o600)

    def test_create_strips_setuid_setgid_sticky_bits(self) -> None:
        if self.use_nfs():
            self.skipTest("privilege bits are only stripped on the FUSE path")
        created = os.path.join(self.mount, "setuid-create")
        os.close(os.open(created, os.O_CREAT | os.O_WRONLY, 0o4755))
        self.assertEqual(0o755, os.lstat(created).st_mode & 0o7777)

        made = os.path.join(self.mount, "setgid-mknod")
        os.mknod(made, stat.S_IFREG | 0o2644)
        self.assertEqual(0o644, os.lstat(made).st_mode & 0o7777)

    def test_statvfs(self) -> None:
        hello_path = os.path.join(self.mount, "hello")
        fs_info = os.statvfs(hello_path)
        self.assertGreaterEqual(fs_info.f_namemax, 255)
        self.assertGreaterEqual(fs_info.f_frsize, 4096)
        self.assertGreaterEqual(fs_info.f_bsize, 4096)

        self.assertGreaterEqual(os.pathconf(hello_path, "PC_NAME_MAX"), 255)
        self.assertGreaterEqual(os.pathconf(hello_path, "PC_PATH_MAX"), 4096)
        self.assertGreaterEqual(os.pathconf(hello_path, "PC_REC_XFER_ALIGN"), 4096)
        self.assertGreaterEqual(os.pathconf(hello_path, "PC_ALLOC_SIZE_MIN"), 4096)
        self.assertGreaterEqual(os.pathconf(hello_path, "PC_REC_MIN_XFER_SIZE"), 4096)
