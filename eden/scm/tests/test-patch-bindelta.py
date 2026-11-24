# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import unittest

import silenttestrunner
from sapling.patch import applybindelta

# A GIT binary delta starts with the sizes of the file it was made against and
# of the result. Its commands then build the result by copying ranges of that
# file or inserting bytes carried in the delta.
BASE = b"0123456789"


def header(srcsize: int, dstsize: int) -> bytes:
    # Each size is a base-128 varint, so a size below 128 is one byte.
    assert srcsize < 0x80 and dstsize < 0x80
    return bytes([srcsize, dstsize])


def copy(offset: int, size: int) -> bytes:
    # 0x80 marks a copy; 0x01 and 0x10 say one offset byte and one size byte follow.
    assert offset < 0x100 and 0 < size < 0x100
    return bytes([0x80 | 0x01 | 0x10, offset, size])


def insert(data: bytes) -> bytes:
    # A command byte from 1 to 127 inserts that many bytes, which follow it.
    assert 0 < len(data) < 0x80
    return bytes([len(data)]) + data


def check(delta: bytes) -> object:
    """Apply delta to BASE; return the result, or the exception as text"""
    try:
        return applybindelta(delta, BASE)
    except Exception as e:
        return f"{type(e).__name__}: {e}"


class ApplyBinDeltaTest(unittest.TestCase):
    def test_copy(self) -> None:
        self.assertEqual(check(header(10, 4) + copy(2, 4)), b"2345")

    def test_malformed(self) -> None:
        corrupt = "PatchError: corrupt binary delta"
        # Shorter than any delta git writes:
        self.assertEqual(check(header(10, 0)), corrupt)
        # The source size never ends, as every byte sets the continuation bit:
        self.assertEqual(check(bytes([0x80 | 10, 0x80, 0x80, 0x80])), corrupt)
        # Copy arguments cut off:
        self.assertEqual(check(header(10, 4) + copy(2, 4)[:-1]), corrupt)
        # Copy past the result size:
        self.assertEqual(check(header(10, 2) + copy(2, 4)), corrupt)
        # Insert past the end of the delta:
        self.assertEqual(check(header(10, 4) + insert(b"abcd")[:-2]), corrupt)
        # Insert past the result size:
        self.assertEqual(check(header(10, 1) + insert(b"ab")), corrupt)
        # Result shorter than its size:
        self.assertEqual(check(header(10, 8) + copy(2, 4)), corrupt)


if __name__ == "__main__":
    silenttestrunner.main(__name__)
