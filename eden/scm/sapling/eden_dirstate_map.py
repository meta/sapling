# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Eden implementation for the dirstatemap class."""

import stat
from typing import BinaryIO, Dict

import bindings

from . import (
    eden_dirstate_serializer,
    EdenThriftClient,
    error,
    localrepo,
    node,
    treestate,
    ui as ui_mod,
    util,
    vfs,
)

parsers = bindings.cext.parsers


MERGE_STATE_NOT_APPLICABLE: int = eden_dirstate_serializer.MERGE_STATE_NOT_APPLICABLE
MERGE_STATE_BOTH_PARENTS: int = eden_dirstate_serializer.MERGE_STATE_BOTH_PARENTS
MERGE_STATE_OTHER_PARENT: int = eden_dirstate_serializer.MERGE_STATE_OTHER_PARENT
DUMMY_MTIME = 0

modefromflag: Dict[str, int] = {
    "": stat.S_IFREG | 0o644,
    "x": stat.S_IFREG | 0o755,
    "l": (stat.S_IFREG if util.iswindows else stat.S_IFLNK) | 0o755,
    "t": stat.S_IFDIR | 0o755,
    "m": 0o160000,
}


class eden_dirstate_map(treestate.treestatemap):
    def __init__(
        self,
        ui: "ui_mod.ui",
        opener: "vfs.abstractvfs",
        root: str,
        thrift_client: "EdenThriftClient.EdenThriftClient",
        repo: "localrepo.localrepository",
    ) -> None:
        self._thrift_client = thrift_client
        self._repo = repo

        # ignore HG_PENDING because identity is used only for writing
        self._identity = util.filestat.frompath(opener.join("dirstate"))

        # Each time we load the treestate, make sure we have the latest
        # version.
        repo._rsrepo.invalidateworkingcopy()

        super().__init__(ui, opener, root, repo._rsrepo.workingcopy().treestate())

    @property
    def identity(self):  # override
        return self._identity

    def _keys(self):
        return self._tree.tracked("")

    def _items(self):
        """Iterate treestate, converting treestate "flags" into legacy merge state enum."""
        for k in self._keys():
            entry = self._tree.get(k, None)
            if entry is None:
                continue
            flags, mode, _, mtime, *_ = entry
            yield (
                k,
                (
                    bindings.treestate.tohgstate(flags),
                    mode,
                    _merge_state_from_flags(flags),
                    mtime,
                ),
            )

    def write(self, st: "BinaryIO", now: int) -> None:  # override
        parents = self.parents()

        # `copymap` walks the treestate on every access.
        copymap = self.copymap

        # Filter out all "clean" entries when writing. (It's possible we should
        # never allow these to be inserted into self._map in the first place.)
        m = {
            k: (v[0], v[1], v[2])
            for k, v in self._items()
            if v[0] != "n" or v[2] != MERGE_STATE_NOT_APPLICABLE or k in copymap
        }
        eden_dirstate_serializer.write(st, parents, m, copymap)
        st.close()

        # Inform the edenfs daemon about the parent change.
        # We do not need to flush any pending transaction state here--manifest
        # and changelog data for a transaction is always written to disk before the
        # dirstate is updated.
        self._thrift_client.setHgParents(
            parents[0],
            parents[1],
        )

    def _read(self, tree):  # override
        self._tree = tree

        metadata = treestate._unpackmetadata(self._tree.getmetadata())

        self._parents = (
            node.bin(metadata.get("p1") or node.nullhex),
            node.bin(metadata.get("p2") or node.nullhex),
        )

        # These shouldn't be needed since we never write out a treestate.
        self._threshold = 0
        self._rootid = 0

    def clear(self):
        # This seems to only be called for EdenFS "hg up -C ...".
        # Let's just manually remove tracked entries since self._tree.reset()
        # doesn't do the right thing with our in-memory treestate.
        self.setparents(node.nullid, node.nullid)
        for k in self._keys():
            self._tree.remove(k)

    def items(self):
        raise RuntimeError("Should not iterate eden_dirstate_map!")

    def __len__(self):
        raise RuntimeError("Should not invoke __len__ on eden_dirstate_map!")

    def __iter__(self):
        raise RuntimeError("Should not invoke __iter__ on eden_dirstate_map!")

    def keys(self):
        raise RuntimeError("Should not invoke keys() on eden_dirstate_map!")

    def __contains__(self, key):
        return self.get(key) is not None

    # For eden we store a sparse dirstate with only added/removed files.
    # For "normal" files, we need to infer their state from the manifest.
    def _get(self, path, default=None):
        entry = super()._get(path, None)
        if entry is not None:
            return entry

        commitctx = self._repo["."]

        try:
            _node, flag = commitctx._fileinfo(path)
        except (KeyError, error.PermissionDeniedError):
            return default

        return (
            bindings.treestate.EXIST_P1 | bindings.treestate.EXIST_NEXT,
            modefromflag[flag],
            MERGE_STATE_NOT_APPLICABLE,
            DUMMY_MTIME,
            None,
        )

    def hastrackeddir(self, d):  # override
        # TODO(mbolin): Unclear whether it is safe to hardcode this to False.
        return False

    def hasdir(self, d):  # override
        # TODO(mbolin): Unclear whether it is safe to hardcode this to False.
        return False


def _merge_state_from_flags(flags):
    # Convert treestate flags back into legacy merge state enum. This mirrors
    # logic in treestate::legacy_eden_dirstate::serialize_entry.
    p1 = flags & bindings.treestate.EXIST_P1
    p2 = flags & bindings.treestate.EXIST_P2
    nxt = flags & bindings.treestate.EXIST_NEXT

    if p2:
        if p1:
            return MERGE_STATE_BOTH_PARENTS
        else:
            return MERGE_STATE_OTHER_PARENT
    elif not p1 and nxt:
        return MERGE_STATE_BOTH_PARENTS
    else:
        return MERGE_STATE_NOT_APPLICABLE
