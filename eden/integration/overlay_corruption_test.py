#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""End-to-end EdenFS behavior for corrupted Legacy overlay data.

Each case starts with regular files `a`, `b`, and `s/f`, whose contents
match their basenames. The test stops EdenFS, corrupts one case directory's
on-disk overlay data, and records the following user-visible results:

  first     start EdenFS with the corrupted data, then walk the directory
  restart   cleanly restart and repeat the walk (`unchanged` if it matches
            `first`), forcing a reload from disk
  archive   corrupt directory inputs preserved by online fsck
  fsck      problems `eden fsck --check-only` attributes to the case
  mv        rename `b` to `b2`, then back to `b`
  rm        delete from the bottom up, stopping at the first error
  recreate  recreate the directory and write/read `new` (`-` if `rm` failed)

`first` follows a clean shutdown and does not run offline fsck. Its walk
loads the directory lazily, so the online-fsck variant may validate or repair
it at that point. Only the `fsck` column runs `eden fsck --check-only`.

`b` begins as a regular file, although a case may make it missing,
unreadable, or bind it as another type. The `mv` phase is intentionally a
round trip: the first rename exercises loading and modifying that entry and
triggers WAL compaction; the second tests the resulting state and restores
`b`, keeping the later `rm` results comparable.

The walk, rename, removal, and recreation all use the mounted filesystem,
not overlay APIs. Removal is a deterministic `rm -rf` equivalent built from
`lstat`, `listdir`, `unlink`, and `rmdir`. The walk neither bypasses nor
explicitly primes or clears kernel/FUSE caches. In the results, `a b s/ s/f`
lists reachable paths; `x:EIO` means accessing `x` failed; `x:"b"` means `x`
contains `b` instead of its own basename; and `x=y` means both paths have the
same inode. `.` is the case directory and `/` is the mount root. `CRASH` means
EdenFS died during the phase; `DOWN` means the mount could not start, so the
phase did not run.

Corrupt WAL bytes are written while EdenFS is stopped, so `first`, `restart`,
and `fsck` observe them before compaction. Later writes are forced to compact
by setting the byte cap to 1. This keeps `mv` and `rm` deterministic, but does
not cover the below-cap path where an append leaves the WAL uncompacted. With
the default 5 MB cap, these small WALs are below the cap and each append uses
a random compaction roll (about 1 in 50 for these cases).

Most cases share a mount. Cases that may crash EdenFS or affect data outside
their directory run alone in an isolated mount, so they cannot disturb other
rows.
"""

from __future__ import annotations

import errno
import json
import os
import pathlib
import re
import stat
import struct
import subprocess
import time
from dataclasses import dataclass
from typing import Callable, Dict, List, Optional, Sequence, Set, Tuple

from eden.fs.cli.util import EdenStartError
from eden.fs.service.eden.thrift_types import (
    MountState,
    RemoveRecursivelyParams,
    SyncBehavior,
)
from eden.integration.lib import testcase

# Mirrors FsInodeCatalog::kHeaderLength.
_HEADER_LEN = 64
# Mirrors enum class WalOpType in eden/fs/inodes/InodeCatalog.h.
_WAL_ADD = 1
_WAL_REMOVE = 2
_ROOT_INODE = 1
_REG: int = stat.S_IFREG | 0o644
_DIR: int = stat.S_IFDIR | 0o755
# Outside DirEntry::kInitialModeMask.
_BAD_MODE: int = 0x0F000000 | _REG
# Well above anything a test allocates; each case adds its index so fsck
# problems can be attributed.
_BIG_INODE: int = 1 << 40

_COLUMNS = ("first", "restart", "archive", "fsck", "mv", "rm", "recreate")
_WIDTHS = (20, 20, 18, 9, 23, 9, 11, 8)

_FSCK_PATTERNS: List[Tuple[re.Pattern[str], str, int]] = [
    (re.compile(r"error reading data for inode (\d+)"), "data", 0),
    (
        re.compile(r"missing overlay file for materialized \w+ inode (-?\d+)"),
        "missing",
        0,
    ),
    (re.compile(r"found orphan \w+ inode (\d+)"), "orphan", 0),
    (re.compile(r"found hard linked inode (\d+)"), "hardlink", 0),
    (re.compile(r"bad stored next inode number: .* at least (\d+)"), "next-ino", -1),
]

# Entries are (name, mode or None to omit the field, inode number).
Entry = Tuple[str, Optional[int], int]


def _varint(n: int) -> bytes:
    out = bytearray()
    while n > 0x7F:
        out.append((n & 0x7F) | 0x80)
        n >>= 7
    out.append(n)
    return bytes(out)


def _zigzag(n: int) -> int:
    return (n << 1) ^ (n >> 63)


def _encode_entry(mode: Optional[int], inode: int) -> bytes:
    # Compact protocol; field 1 is i32 mode and field 2 is i64 inodeNumber.
    out = b""
    last = 0
    if mode is not None:
        out += b"\x15" + _varint(_zigzag(mode))
        last = 1
    out += bytes([((2 - last) << 4) | 6]) + _varint(_zigzag(inode))
    return out + b"\x00"


def _encode_dir(entries: Sequence[Entry], extra_fields: bytes = b"") -> bytes:
    # Hand-rolled so tests can express data the thrift serializer never
    # emits: duplicate keys and missing fields.
    out = b"\x1b"
    if entries:
        out += _varint(len(entries)) + b"\x8c"
        for name, mode, inode in entries:
            encoded = name.encode()
            out += _varint(len(encoded)) + encoded + _encode_entry(mode, inode)
    else:
        out += b"\x00"
    return out + extra_fields + b"\x00"


def _wal_add(name: str, mode: int, inode: int) -> bytes:
    encoded = name.encode()
    payload = struct.pack(
        f"<BH{len(encoded)}siqB", _WAL_ADD, len(encoded), encoded, mode, inode, 0
    )
    return struct.pack("<I", len(payload)) + payload


def _wal_remove(name: str, extra: bytes = b"") -> bytes:
    encoded = name.encode()
    payload = struct.pack(f"<BH{len(encoded)}s", _WAL_REMOVE, len(encoded), encoded)
    payload += extra
    return struct.pack("<I", len(payload)) + payload


def _errname(e: OSError) -> str:
    return errno.errorcode.get(e.errno or 0, str(e.errno))


class Ctx:
    """A case dir laid out as `a`, `b`, `s/`, `s/f`, with its inodes."""

    def __init__(
        self, overlay_dir: pathlib.Path, ino: Dict[str, int], big: int
    ) -> None:
        self.overlay_dir = overlay_dir
        self.ino = ino
        self.big = big

    def path(self, inode: int, suffix: str = "") -> pathlib.Path:
        return self.overlay_dir / f"{inode % 256:02x}" / f"{inode}{suffix}"

    def entries(self, **replace_b: Entry) -> List[Entry]:
        entries: List[Entry] = [
            ("a", _REG, self.ino["a"]),
            ("b", _REG, self.ino["b"]),
            ("s", _DIR, self.ino["s"]),
        ]
        if "b" in replace_b:
            entries[1] = replace_b["b"]
        return entries

    def write_dir(self, body: bytes, dir_name: str = ".") -> None:
        path = self.path(self.ino[dir_name])
        header = path.read_bytes()[:_HEADER_LEN]
        path.write_bytes(header + body)
        self.path(self.ino[dir_name], ".wal").unlink(missing_ok=True)

    def append_wal(
        self, data: bytes, dir_name: str = ".", base: Optional[List[Entry]] = None
    ) -> None:
        # WAL compaction is randomized, so an entry may live in either the
        # base or the daemon's WAL. Start from a known base and no WAL.
        if base is None:
            base = self.entries() if dir_name == "." else [("f", _REG, self.ino["s/f"])]
        self.write_dir(_encode_dir(base), dir_name)
        with self.path(self.ino[dir_name], ".wal").open("ab") as f:
            f.write(data)

    def corrupt_file(self, name: str, fn: Callable[[pathlib.Path], None]) -> None:
        fn(self.path(self.ino[name]))


class _MountDown(Exception):
    pass


@dataclass
class Case:
    name: str
    build: Callable[[Ctx], None]
    isolate: bool = False


def _truncate(path: pathlib.Path) -> None:
    os.truncate(path, 0)


def _bad_header(path: pathlib.Path) -> None:
    with path.open("r+b") as f:
        f.write(b"XXXX")


def _unlink_dir(path: pathlib.Path) -> None:
    path.unlink()
    path.with_name(path.name + ".wal").unlink(missing_ok=True)


# Unknown field 15 (i32) in OverlayDir; old binaries used to skip these.
_UNKNOWN_FIELD = b"\xe5" + _varint(7 << 1)

CASES: List[Case] = [
    Case("control", lambda c: c.write_dir(_encode_dir(c.entries()))),
    # Directory base file, entry `b` replaced.
    Case(
        "base_bad_mode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _BAD_MODE, c.ino["b"])))),
    ),
    Case(
        "base_no_mode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", None, c.ino["b"])))),
    ),
    Case(
        "base_big_inode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _REG, c.big)))),
    ),
    Case(
        "base_root_inode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _DIR, _ROOT_INODE)))),
    ),
    Case(
        "base_zero_inode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _REG, 0)))),
    ),
    Case(
        "base_bad_name",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("x/y", _REG, c.ino["b"])))),
    ),
    Case(
        "base_dup_name",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("a", _REG, c.ino["b"])))),
    ),
    Case(
        "base_dup_inode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _REG, c.ino["a"])))),
    ),
    Case(
        "base_self_ref",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _DIR, c.ino["."])))),
    ),
    Case(
        "base_unknown_field",
        lambda c: c.write_dir(_encode_dir(c.entries(), _UNKNOWN_FIELD)),
    ),
    Case(
        "base_trailing", lambda c: c.write_dir(_encode_dir(c.entries()) + b"\x00\x00")
    ),
    Case(
        "base_negative_inode",
        lambda c: c.write_dir(_encode_dir(c.entries(b=("b", _REG, -5)))),
    ),
    Case("base_no_stop", lambda c: c.write_dir(_encode_dir(c.entries())[:-1])),
    # Map header (3 bytes) and all of `a`, then `b`'s name and first field
    # header.
    Case(
        "base_truncated",
        lambda c: c.write_dir(
            _encode_dir(c.entries())[: 3 + 2 + len(_encode_entry(_REG, c.ino["a"])) + 3]
        ),
    ),
    # `p` in `s` points back at the case dir.
    Case(
        "base_cycle_x",
        lambda c: c.write_dir(
            _encode_dir([("f", _REG, c.ino["s/f"]), ("p", _DIR, c.ino["."])]), "s"
        ),
    ),
    # `g` in `s` binds file `a` as a directory.
    Case(
        "base_mode_conflict_x",
        lambda c: c.write_dir(
            _encode_dir([("f", _REG, c.ino["s/f"]), ("g", _DIR, c.ino["a"])]), "s"
        ),
    ),
    # WAL appended to a base holding the whole directory.
    Case("wal_bad_mode", lambda c: c.append_wal(_wal_add("b", _BAD_MODE, c.ino["b"]))),
    Case("wal_big_inode", lambda c: c.append_wal(_wal_add("b", _REG, c.big))),
    # The valid binding of `b` is only in the WAL, before the bad one.
    Case(
        "wal_shadow_big_inode",
        lambda c: c.append_wal(
            _wal_add("b", _REG, c.ino["b"]) + _wal_add("b", _REG, c.big),
            base=[e for e in c.entries() if e[0] != "b"],
        ),
    ),
    Case(
        "wal_root_inode",
        lambda c: c.append_wal(_wal_add("b", _DIR, _ROOT_INODE)),
    ),
    Case("wal_zero_inode", lambda c: c.append_wal(_wal_add("b", _REG, 0))),
    Case("wal_negative_inode", lambda c: c.append_wal(_wal_add("b", _REG, -5))),
    Case(
        "wal_bad_name",
        lambda c: c.append_wal(_wal_add("x/y", _REG, c.ino["b"]) + _wal_remove("b")),
    ),
    # A rename that crashed between the destination ADD and the source REMOVE.
    Case(
        "wal_rename_crash",
        lambda c: c.append_wal(_wal_add("c", _REG, c.ino["a"])),
    ),
    # Same, with the destination name sorting before the source.
    Case(
        "wal_rename_crash_rev",
        lambda c: c.append_wal(_wal_add("0", _REG, c.ino["a"])),
    ),
    Case(
        "wal_rename_crash_x",
        lambda c: c.append_wal(_wal_add("g", _REG, c.ino["a"]), "s"),
    ),
    Case(
        "wal_self_ref",
        lambda c: c.append_wal(_wal_add("b", _DIR, c.ino["."])),
    ),
    # A complete frame followed by a torn one.
    Case(
        "wal_torn",
        lambda c: c.append_wal(_wal_remove("a") + _wal_add("c", _REG, c.ino["b"])[:-3]),
    ),
    # A complete frame followed by part of a length prefix.
    Case(
        "wal_torn_len",
        lambda c: c.append_wal(_wal_remove("a") + _wal_add("c", _REG, c.ino["b"])[:2]),
    ),
    Case("wal_remove_payload", lambda c: c.append_wal(_wal_remove("a", b"\0"))),
    # File and subdirectory overlay data.
    Case("file_bad_header", lambda c: c.corrupt_file("b", _bad_header)),
    Case("file_truncated", lambda c: c.corrupt_file("b", _truncate)),
    Case("file_missing", lambda c: c.corrupt_file("b", pathlib.Path.unlink)),
    Case("subdir_truncated", lambda c: c.corrupt_file("s", _truncate)),
    Case("subdir_missing", lambda c: c.corrupt_file("s", _unlink_dir)),
]

EXPECTED_ONLINE_FSCK_OFF = """
case                 | first                | restart            | archive   | fsck                    | mv        | rm          | recreate
control              | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
base_bad_mode        | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_no_mode         | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_big_inode       | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_root_inode      | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_zero_inode      | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_bad_name        | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_dup_name        | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_dup_inode       | a b:EIO s/ s/f       | unchanged          | -         | hardlink,orphan         | ok        | a:EIO       | -
base_self_ref        | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_unknown_field   | .:EIO                | unchanged          | -         | data,orphan             | b:EIO     | .:EIO       | -
base_trailing        | .:EIO                | unchanged          | -         | data,orphan             | b:EIO     | .:EIO       | -
base_negative_inode  | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
base_no_stop         | .:EIO                | unchanged          | -         | data,orphan             | b:EIO     | .:EIO       | -
base_truncated       | .:EIO                | unchanged          | -         | data,orphan             | b:EIO     | .:EIO       | -
base_cycle_x         | a b s/ s/f s/p:EIO   | unchanged          | -         | hardlink                | ok        | s/p:EIO     | -
base_mode_conflict_x | a b s/ s/f s/g:EIO   | unchanged          | -         | hardlink                | ok        | ok          | ok
wal_bad_mode         | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
wal_big_inode        | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
wal_shadow_big_inode | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
wal_root_inode       | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
wal_zero_inode       | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
wal_negative_inode   | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
wal_bad_name         | a s/ s/f             | unchanged          | -         | orphan                  | b:ENOENT  | ok          | ok
wal_rename_crash     | b c:"a" s/ s/f       | unchanged          | -         | -                       | ok        | ok          | ok
wal_rename_crash_rev | 0:"a" b s/ s/f       | unchanged          | -         | -                       | ok        | ok          | ok
wal_rename_crash_x   | a b s/ s/f s/g:EIO   | unchanged          | -         | hardlink                | ok        | ok          | ok
wal_self_ref         | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
wal_torn             | b s/ s/f             | unchanged          | -         | orphan                  | ok        | ok          | ok
wal_torn_len         | b s/ s/f             | unchanged          | -         | orphan                  | ok        | ok          | ok
wal_remove_payload   | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
file_bad_header      | a b s/ s/f           | unchanged          | -         | data                    | ok        | ok          | ok
file_truncated       | a b:EIO s/ s/f       | unchanged          | -         | data                    | ok        | b:EIO       | -
file_missing         | a b:ENOENT s/ s/f    | unchanged          | -         | missing                 | ok        | b:ENOENT    | -
subdir_truncated     | a b s:EIO            | unchanged          | -         | data,orphan             | ok        | s:EIO       | -
subdir_missing       | a b s/               | unchanged          | -         | missing,orphan          | ok        | ok          | ok
"""

EXPECTED_ONLINE_FSCK_ON = """
case                 | first                | restart            | archive   | fsck                    | mv        | rm          | recreate
control              | a b s/ s/f           | unchanged          | -         | -                       | ok        | ok          | ok
base_bad_mode        | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_no_mode         | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_big_inode       | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_root_inode      | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_zero_inode      | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_bad_name        | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_dup_name        | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_dup_inode       | a b:"" s/ s/f        | unchanged          | -         | orphan                  | ok        | ok          | ok
base_self_ref        | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_unknown_field   | a b s/ s/f           | unchanged          | dir       | -                       | ok        | ok          | ok
base_trailing        | a b s/ s/f           | unchanged          | dir       | -                       | ok        | ok          | ok
base_negative_inode  | a s/ s/f             | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_no_stop         | a b s/ s/f           | unchanged          | dir       | -                       | ok        | ok          | ok
base_truncated       | a                    | unchanged          | dir       | orphan                  | b:ENOENT  | ok          | ok
base_cycle_x         | a b s/ s/f s/p/      | unchanged          | -         | -                       | ok        | ok          | ok
base_mode_conflict_x | a b s/ s/f s/g/      | unchanged          | -         | -                       | ok        | ok          | ok
wal_bad_mode         | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
wal_big_inode        | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
wal_shadow_big_inode | a s/ s/f             | unchanged          | dir,wal   | orphan                  | b:ENOENT  | ok          | ok
wal_root_inode       | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
wal_zero_inode       | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
wal_negative_inode   | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
wal_bad_name         | a s/ s/f             | unchanged          | dir,wal   | orphan                  | b:ENOENT  | ok          | ok
wal_rename_crash     | b c:"a" s/ s/f       | unchanged          | -         | -                       | ok        | ok          | ok
wal_rename_crash_rev | 0:"a" b s/ s/f       | unchanged          | -         | -                       | ok        | ok          | ok
wal_rename_crash_x   | a b s/ s/f s/g:""    | unchanged          | -         | -                       | ok        | ok          | ok
wal_self_ref         | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
wal_torn             | b s/ s/f             | unchanged          | dir,wal   | orphan                  | ok        | ok          | ok
wal_torn_len         | b s/ s/f             | unchanged          | dir,wal   | orphan                  | ok        | ok          | ok
wal_remove_payload   | a b s/ s/f           | unchanged          | dir,wal   | -                       | ok        | ok          | ok
file_bad_header      | a b s/ s/f           | unchanged          | -         | data                    | ok        | ok          | ok
file_truncated       | a b:EACCES s/ s/f    | unchanged          | -         | data                    | ok        | ok          | ok
file_missing         | a b:EACCES s/ s/f    | unchanged          | -         | missing                 | ok        | ok          | ok
subdir_truncated     | a b s/               | unchanged          | dir       | orphan                  | ok        | ok          | ok
subdir_missing       | a b s/               | unchanged          | -         | missing,orphan          | ok        | ok          | ok
"""


class OverlayCorruptionTestBase(testcase.HgRepoTestMixin, testcase.EdenRepoTest):
    online_fsck: bool = False
    expected: str = ""

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = super().edenfs_extra_config() or {}
        overlay = configs.setdefault("overlay", [])
        overlay.append("use-wal = true")
        # Compact on every append. Compaction is otherwise a random roll, and
        # it validates every entry, so rows would flip between runs.
        overlay.append('wal-compaction-byte-cap = "1"')
        overlay.append(f"online-fsck = {str(self.online_fsck).lower()}")
        return configs

    def populate_repo(self) -> None:
        self.repo.write_file("README", "overlay corruption\n")
        self.repo.commit("Initial commit.")

    def _start(self, mount: pathlib.Path) -> None:
        try:
            self.eden.start()
        except EdenStartError:
            raise _MountDown()
        deadline = time.monotonic() + 30
        while (state := self.eden.get_mount_state(mount)) != MountState.RUNNING:
            # edenfs drops a mount that fails to initialize from its list.
            if state in (None, MountState.FUSE_ERROR, MountState.INIT_ERROR):
                raise _MountDown()
            self.assertLess(time.monotonic(), deadline, "mount did not start")
            time.sleep(0.02)

    def _restart(self, mount: pathlib.Path) -> None:
        if self.online_fsck:
            self.eden.kill_dirty()
        else:
            self.eden.shutdown()
        self._start(mount)

    def _forget(self, mount: pathlib.Path) -> None:
        # `eden remove` needs the mount to load, which this one does not.
        if self.eden.is_healthy():
            self.eden.shutdown()
        else:
            self.eden.wait_for_exit()
        config = self.eden.eden_dir / "config.json"
        mounts = json.loads(config.read_text())
        del mounts[str(mount)]
        config.write_text(json.dumps(mounts))

    def _make_ctx(self, mount: pathlib.Path, name: str, big: int) -> Ctx:
        d = mount / name
        (d / "s").mkdir(parents=True)
        (d / "a").write_text("a")
        (d / "b").write_text("b")
        (d / "s" / "f").write_text("f")
        rels = {".": d, "a": d / "a", "b": d / "b", "s": d / "s", "s/f": d / "s" / "f"}
        ino = {k: os.lstat(p).st_ino for k, p in rels.items()}
        return Ctx(self.eden.overlay_dir_for_mount(mount), ino, big)

    def _walk(self, case_dir: pathlib.Path) -> str:
        items: List[str] = []
        seen: Dict[int, str] = {_ROOT_INODE: "/"}

        def visit(path: pathlib.Path, rel: str) -> None:
            label = rel or "."
            try:
                st = os.lstat(path)
                if st.st_ino in seen:
                    items.append(f"{label}={seen[st.st_ino]}")
                    return
                seen[st.st_ino] = label
                is_dir = stat.S_ISDIR(st.st_mode)
                children = sorted(os.listdir(path)) if is_dir else []
                data = b"" if is_dir else path.read_bytes()
            except OSError as e:
                items.append(f"{label}:{_errname(e)}")
                return
            if not is_dir and data != path.name.encode():
                items.append(f'{label}:"{data[:8].decode(errors="replace")}"')
            elif rel:
                items.append(rel + ("/" if is_dir else ""))
            for child in children:
                visit(path / child, f"{rel}{child}" if not rel else f"{rel}/{child}")

        visit(case_dir, "")
        return " ".join(items) or "(empty)"

    def _rm(self, case_dir: pathlib.Path) -> str:
        seen: Set[int] = {_ROOT_INODE}

        def remove(path: pathlib.Path) -> None:
            st = os.lstat(path)
            if not stat.S_ISDIR(st.st_mode):
                path.unlink()
                return
            # An aliased directory (cycle or duplicate) is not descended into.
            if st.st_ino not in seen:
                seen.add(st.st_ino)
                for child in sorted(os.listdir(path)):
                    remove(path / child)
            path.rmdir()

        try:
            remove(case_dir)
        except OSError as e:
            rel = os.path.relpath(e.filename or case_dir, case_dir)
            return f"{rel}:{_errname(e)}"
        return "ok"

    def _mv(self, case_dir: pathlib.Path) -> str:
        try:
            os.rename(case_dir / "b", case_dir / "b2")
            os.rename(case_dir / "b2", case_dir / "b")
        except OSError as e:
            return f"{os.path.basename(e.filename)}:{_errname(e)}"
        return "ok"

    def _recreate(self, case_dir: pathlib.Path) -> str:
        try:
            case_dir.mkdir()
            (case_dir / "new").write_text("new")
            (case_dir / "new").read_text()
        except OSError as e:
            return _errname(e)
        return "ok"

    def _archive(self, ctx: Ctx) -> str:
        repair_root = ctx.overlay_dir.parent / "fsck"
        if not repair_root.exists():
            return "-"
        inodes = set(ctx.ino.values())
        kinds: Set[str] = set()
        for repair_dir in repair_root.iterdir():
            for path in repair_dir.iterdir():
                match = re.fullmatch(r"(-?\d+)\.(dir|wal)(?:\.\d+)?", path.name)
                if match and int(match.group(1)) in inodes:
                    kinds.add(match.group(2))
        return ",".join(sorted(kinds)) or "-"

    def _fsck(self, mount: pathlib.Path, ctxs: Dict[str, Ctx]) -> Dict[str, str]:
        result = self.eden.run_unchecked(
            "fsck",
            str(mount),
            "--check-only",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        if result.returncode < 0:
            return dict.fromkeys(ctxs, "CRASH")
        output = result.stdout.decode("utf-8", errors="replace")
        owner = {
            n: name for name, ctx in ctxs.items() for n in (*ctx.ino.values(), ctx.big)
        }
        problems: Dict[str, Set[str]] = {name: set() for name in ctxs}
        count = 0
        for line in output.splitlines():
            _, sep, msg = line.partition(": error: ")
            if not sep:
                continue
            count += 1
            kind, inodes = "other", [int(n) for n in re.findall(r"-?\d+", msg)]
            for pattern, k, adjust in _FSCK_PATTERNS:
                m = pattern.search(msg)
                if m:
                    kind, inodes = k, [int(m.group(1)) + adjust]
                    break
            # fsck prints inode numbers as uint64.
            inodes = [n - (1 << 64) if n >= 1 << 63 else n for n in inodes]
            name = next((owner[n] for n in inodes if n in owner), None)
            if name is None:
                # The inode number may be one the daemon allocated while
                # loading the corrupted entry; the path still names the case.
                paths = re.findall(r"[( ](\w+)/", msg)
                name = next((p for p in paths if p in problems), None)
            if name is None and len(problems) == 1:
                # An isolated mount holds a single case.
                (name,) = problems
            if name is None:
                self.fail(f"unattributed fsck problem {msg!r}:\n{output}")
            problems[name].add(kind)
        m = re.search(r"found (\d+) problems|no problems found", output)
        self.assertIsNotNone(m, output)
        self.assertEqual(int(m.group(1) or 0), count, output)
        return {
            name: ",".join(sorted(kinds)) or "-" for name, kinds in problems.items()
        }

    def _phase(
        self,
        mount: pathlib.Path,
        results: Dict[str, Dict[str, str]],
        column: str,
        values: Dict[str, str],
    ) -> None:
        crashed = not self.eden.is_healthy()
        if crashed and len(values) > 1:
            self.fail(f"edenfs died during {column}: {values}; mark the case isolate")
        for name, value in values.items():
            results[name][column] = "CRASH" if crashed else value
        if crashed:
            self.eden.wait_for_exit()
            self._start(mount)

    def _run(
        self,
        mount: pathlib.Path,
        ctxs: Dict[str, Ctx],
        results: Dict[str, Dict[str, str]],
    ) -> None:
        dirs = {name: mount / name for name in ctxs}
        self.eden.shutdown()
        for c in CASES:
            if c.name in ctxs:
                c.build(ctxs[c.name])
        self._start(mount)
        if self.online_fsck:
            # Prime the reservation, then exercise a dirty startup without
            # accessing any case directory first.
            self._restart(mount)
        walk = {n: self._walk(d) for n, d in dirs.items()}
        self._phase(mount, results, "first", walk)

        self._restart(mount)
        walk = {n: self._walk(d) for n, d in dirs.items()}
        self._phase(mount, results, "restart", walk)

        self.eden.shutdown()
        archive = {n: self._archive(ctx) for n, ctx in ctxs.items()}
        fsck = self._fsck(mount, ctxs)
        self._start(mount)
        if self.online_fsck:
            self._restart(mount)
        self._phase(mount, results, "archive", archive)
        self._phase(mount, results, "fsck", fsck)

        mv = {n: self._mv(d) for n, d in dirs.items()}
        self._phase(mount, results, "mv", mv)
        rm = {n: self._rm(d) for n, d in dirs.items()}
        self._phase(mount, results, "rm", rm)
        recreate = {
            n: self._recreate(d) if results[n]["rm"] == "ok" else "-"
            for n, d in dirs.items()
        }
        self._phase(mount, results, "recreate", recreate)

    def test_corruption_matrix(self) -> None:
        results: Dict[str, Dict[str, str]] = {c.name: {} for c in CASES}
        bigs = {c.name: _BIG_INODE + i for i, c in enumerate(CASES)}
        shared = {
            c.name: self._make_ctx(self.mount_path, c.name, bigs[c.name])
            for c in CASES
            if not c.isolate
        }
        self._run(self.mount_path, shared, results)
        for c in CASES:
            if c.isolate:
                mount = pathlib.Path(self.mounts_dir) / c.name
                self.eden.clone(self.repo.path, mount)
                ctx = self._make_ctx(mount, c.name, bigs[c.name])
                try:
                    self._run(mount, {c.name: ctx}, results)
                except _MountDown:
                    for column in _COLUMNS:
                        results[c.name].setdefault(column, "DOWN")
                    self._forget(mount)
                    self._start(self.mount_path)
                else:
                    # Leftover damage would crash later daemon starts.
                    self.eden.remove(str(mount))

        for result in results.values():
            if result["restart"] == result["first"]:
                result["restart"] = "unchanged"

        rows = [("case", *_COLUMNS)] + [
            (c.name, *(results[c.name][col] for col in _COLUMNS)) for c in CASES
        ]
        table = "\n".join(
            " | ".join(v.ljust(w) for v, w in zip(row, _WIDTHS)).rstrip()
            for row in rows
        )
        print(f"\n{table}\n")
        self.maxDiff = None
        self.assertEqual(self.expected.strip(), table)

    def test_overflowing_inode_reservation(self) -> None:
        self.eden.shutdown()
        reservation = (
            self.eden.overlay_dir_for_mount(self.mount_path) / "inode-reservation"
        )
        reservation.unlink()
        overflow = (1 << 64) - 1
        reservation.symlink_to(str(overflow))
        self.assertEqual(overflow, int(reservation.readlink().name))

        # Restart will detect the bad reservation and restore it.
        self._start(self.mount_path)
        self.eden.shutdown()
        self.assertNotEqual(overflow, int(reservation.readlink().name))

    async def test_remove_recursively_unloaded_subtree(self) -> None:
        root = self.mount_path / "disk_only"
        path = root / "s"
        path.mkdir(parents=True)
        (path / "f").write_text("f")
        inode = os.lstat(path).st_ino
        ctx = Ctx(
            self.eden.overlay_dir_for_mount(self.mount_path),
            {".": inode},
            _BIG_INODE,
        )

        # Restart so removeRecursively sees the child subtree only on disk.
        if self.online_fsck:
            self.eden.kill_dirty()
        else:
            self.eden.shutdown()
        self._start(self.mount_path)
        async with self.get_async_thrift_client() as client:
            await client.removeRecursively(
                RemoveRecursivelyParams(
                    mountPoint=os.fsencode(self.mount_path),
                    path=b"disk_only",
                    sync=SyncBehavior(),
                )
            )
        self.eden.shutdown()

        fsck = self._fsck(self.mount_path, {"disk_only": ctx})["disk_only"]
        self.assertFalse(ctx.path(inode).exists())
        self.assertEqual("-", fsck)


@testcase.eden_nfs_repo_test(run_io_uring=False)
class UnreadableRuntimeInodeTest(testcase.HgRepoTestMixin, testcase.EdenRepoTest):
    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = super().edenfs_extra_config() or {}
        configs.setdefault("overlay", []).append("online-fsck = true")
        return configs

    def select_storage_engine(self) -> str:
        return "sqlite"

    def populate_repo(self) -> None:
        self.repo.write_file("README", "overlay corruption\n")
        self.repo.commit("Initial commit.")

    async def test_unreadable_runtime_inode_is_removable(self) -> None:
        path = self.mount_path / "unreadable"
        path.write_text("data")
        inode = os.lstat(path).st_ino
        overlay = self.eden.overlay_dir_for_mount(self.mount_path)
        Ctx(overlay, {".": inode}, _BIG_INODE).path(inode).unlink()

        async with self.get_async_thrift_client() as client:
            await client.invalidateKernelInodeCache(os.fsencode(self.mount_path), b"")
        self.assertEqual(inode, os.lstat(path).st_ino)
        path.unlink()
        with self.assertRaises(FileNotFoundError):
            os.lstat(path)


class OnlineFsckOffTest(OverlayCorruptionTestBase):
    expected: str = EXPECTED_ONLINE_FSCK_OFF


class OnlineFsckOnTest(OverlayCorruptionTestBase):
    online_fsck: bool = True
    expected: str = EXPECTED_ONLINE_FSCK_ON

    def _make_aliased_subtrees(self) -> Tuple[pathlib.Path, pathlib.Path, Ctx]:
        source = self.mount_path / "A" / "x"
        alias = self.mount_path / "B" / "y"
        source.mkdir(parents=True)
        (source / "f").write_text("contents")
        alias.mkdir(parents=True)
        source_inode = os.lstat(source).st_ino
        parent_inode = os.lstat(alias.parent).st_ino
        ctx = Ctx(
            self.eden.overlay_dir_for_mount(self.mount_path),
            {".": parent_inode, "x": source_inode},
            _BIG_INODE,
        )
        self.eden.shutdown()
        ctx.write_dir(_encode_dir([("y", _DIR, source_inode)]))
        self._start(self.mount_path)
        return source, alias, ctx

    async def test_remove_unseen_alias_reclaims_payload(self) -> None:
        source, alias, ctx = self._make_aliased_subtrees()
        # Neither x nor y has been loaded since the restart. Normal removal
        # may reclaim their shared payload; y will subsequently load as empty.
        async with self.get_async_thrift_client() as client:
            await client.removeRecursively(
                RemoveRecursivelyParams(
                    mountPoint=os.fsencode(self.mount_path),
                    path=b"A",
                    sync=SyncBehavior(),
                )
            )
        self.eden.shutdown()
        self.assertFalse(ctx.path(ctx.ino["x"]).exists())
        self._start(self.mount_path)
        self.assertFalse(source.exists())
        self.assertEqual([], os.listdir(alias))
        alias.rmdir()

    def test_remove_recovered_alias_preserves_loaded_subtree(self) -> None:
        source, alias, ctx = self._make_aliased_subtrees()
        self.assertEqual("contents", (source / "f").read_text())
        # Loading y after x detects the conflict. Its empty replacement must
        # have its own inode so removing y cannot remove x's backing data.
        self.assertEqual([], os.listdir(alias))
        self.assertNotEqual(ctx.ino["x"], os.lstat(alias).st_ino)
        self.eden.shutdown()
        self._start(self.mount_path)
        # Read y first: the repaired binding must survive unloading/restart,
        # rather than making the next lookup choose a different owner of x.
        self.assertEqual([], os.listdir(alias))
        alias.rmdir()
        self.assertEqual("contents", (source / "f").read_text())

    def test_repaired_symlink_is_removable_without_redirecting_traversal(self) -> None:
        source = self.mount_path / "A" / "x"
        alias = self.mount_path / "B" / "y"
        source.parent.mkdir()
        alias.parent.mkdir()
        source.symlink_to("original-target")
        alias.symlink_to("other-target")
        victim = alias.parent / "victim"
        victim.write_text("contents")
        source_inode = os.lstat(source).st_ino
        ctx = Ctx(
            self.eden.overlay_dir_for_mount(self.mount_path),
            {".": os.lstat(alias.parent).st_ino},
            _BIG_INODE,
        )
        victim_inode = os.lstat(victim).st_ino
        self.eden.shutdown()
        ctx.write_dir(
            _encode_dir(
                [
                    ("y", stat.S_IFLNK | 0o777, source_inode),
                    ("victim", _REG, victim_inode),
                ]
            )
        )
        self._start(self.mount_path)
        self.assertEqual("original-target", os.readlink(source))
        self.assertNotEqual(source_inode, os.lstat(alias).st_ino)
        self.eden.shutdown()
        self._start(self.mount_path)

        with self.assertRaises(OSError) as read_error:
            os.readlink(alias)
        self.assertEqual(errno.EIO, read_error.exception.errno)
        with self.assertRaises(OSError) as unlink_error:
            (alias / "victim").unlink()
        self.assertEqual(errno.EIO, unlink_error.exception.errno)
        self.assertEqual("contents", victim.read_text())
        alias.unlink()
        self.assertEqual("original-target", os.readlink(source))

    def test_offline_fsck_repairs_duplicate_bindings(self) -> None:
        # Exercise offline repair before online lookup rewrites these bindings.
        # They no longer fail normal removal with online fsck enabled.
        names = {"base_dup_inode", "base_cycle_x"}
        cases = [case for case in CASES if case.name in names]
        self.assertEqual(names, {case.name for case in cases})
        ctxs = {
            case.name: self._make_ctx(self.mount_path, case.name, _BIG_INODE + index)
            for index, case in enumerate(cases)
        }

        self.eden.shutdown()
        for case in cases:
            case.build(ctxs[case.name])
        repair = self.eden.run_unchecked(
            "fsck",
            str(self.mount_path),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.assertIn(
            "successfully repaired all",
            repair.stdout.decode("utf-8", errors="replace"),
        )
        self.assertEqual(dict.fromkeys(ctxs, "-"), self._fsck(self.mount_path, ctxs))

        self._start(self.mount_path)
        removed = {name: self._rm(self.mount_path / name) for name in sorted(ctxs)}
        self.assertEqual(dict.fromkeys(ctxs, "ok"), removed)
        recreated = {
            name: self._recreate(self.mount_path / name) for name in sorted(ctxs)
        }
        self.assertEqual(dict.fromkeys(ctxs, "ok"), recreated)


class NfsReaddirplusCorruptionTest(
    testcase.NFSTestMixin, testcase.HgRepoTestMixin, testcase.EdenRepoTest
):
    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = super().edenfs_extra_config() or {}
        overlay = configs.setdefault("overlay", [])
        overlay.append("use-wal = true")
        overlay.append("online-fsck = true")
        return configs

    def select_storage_engine(self) -> str:
        return "sqlite"

    def populate_repo(self) -> None:
        self.repo.write_file("README", "overlay corruption\n")
        self.repo.commit("Initial commit.")

    def test_readdirplus_withholds_conflicting_handle(self) -> None:
        case_dir = self.mount_path / "nfs"
        (case_dir / "s").mkdir(parents=True)
        (case_dir / "a").write_text("a")
        (case_dir / "b").write_text("b")
        (case_dir / "s" / "f").write_text("f")
        paths = {
            ".": case_dir,
            "a": case_dir / "a",
            "b": case_dir / "b",
            "s": case_dir / "s",
            "s/f": case_dir / "s" / "f",
        }
        ctx = Ctx(
            self.eden.overlay_dir_for_mount(self.mount_path),
            {name: os.lstat(path).st_ino for name, path in paths.items()},
            _BIG_INODE,
        )
        self.eden.kill_dirty()
        ctx.append_wal(_wal_add("g", _REG, ctx.ino["a"]), "s")
        self.eden.start()

        os.lstat(self.mount_path / "nfs" / "a")
        subdir = self.mount_path / "nfs" / "s"
        names = " ".join(sorted(os.listdir(subdir)))
        try:
            alias = os.lstat(subdir / "g")
        except OSError as ex:
            lookup = _errname(ex)
        else:
            lookup = "alias" if alias.st_ino == ctx.ino["a"] else "fresh"

        expected = """readdirplus | lookup-g
f g         | fresh"""
        self.assertEqual(expected, f"readdirplus | lookup-g\n{names:<11} | {lookup}")
        # The repaired lookup can return a new inode, but readdirplus must not
        # give g the conflicting handle for a. Their payloads stay separate.
        self.assertEqual("", (subdir / "g").read_text())
        self.assertEqual("a", (case_dir / "a").read_text())


# Keep the shared base class out of unittest discovery
del OverlayCorruptionTestBase
