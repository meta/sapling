# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import time as mtime
from dataclasses import dataclass
from typing import Optional

from sapling import error, perftrace, util
from sapling.edenapi_upload import filetypefromfile
from sapling.ext.commitcloud import backupstate, upload as ccupload
from sapling.i18n import _
from sapling.node import nullid
from sapling.revset import parseage

from .metalog import (
    fetchlatestbubble,
    fetchlatestsnapshot,
    getcsidbubblemapping,
    SnapshotMetadata,
    storelatest,
    storesnapshotmetadata,
)
from .update import fetchsnapshot

# Binary conversion constant
MIB_TO_BYTES = 1048576  # 1 MiB = 1,048,576 bytes


def getdefaultmaxuntrackedsize(ui):
    """Get the default maximum untracked file size in bytes from config.

    Default is 1GB if not configured.
    """
    return ui.configbytes("snapshot", "maxuntrackedsize", "1GB")


@util.timefunction("snapshot_backup_parents", 0, "ui")
def _backupparents(repo, parents) -> None:
    """make sure this commit's ancestors are backed up in commitcloud"""

    # Check local backup state first to avoid unnecessary work
    localbackupstate = backupstate.BackupState(repo)
    backedup = localbackupstate.backedup

    # Early return if parents are already backed up locally
    if parents[0] in backedup and (parents[1] == nullid or parents[1] in backedup):
        return

    # pyre-fixme[10]: Name `ancestors` is used but not defined.
    # pyre-fixme[10]: Name `draft` is used but not defined.
    draftnodes = repo.dageval(lambda: draft() & ancestors(parents))

    if not draftnodes:
        return

    # Use commit cloud's upload function which checks local backup state first
    # before issuing "lookup" to the server to avoid unnecessary network calls
    heads = repo.nodes("heads(%ln)", draftnodes)

    (_uploaded, failed) = ccupload.upload(
        repo, repo.revs("%ln", heads), localbackupstate=localbackupstate
    )

    if failed:
        raise error.Abort(
            _("failed to upload ancestors to commit cloud: {}").format(
                [repo[node].hex() for node in failed]
            )
        )


def _parselifetime(opts):
    if opts["lifetime"] != "":
        return parseage(opts["lifetime"])
    else:
        return None


def parselabels(opts):
    if opts["labels"] != "":
        return [label.strip() for label in opts["labels"].split(",") if label.strip()]
    else:
        return None


def mib_to_bytes(mib: float) -> int:
    """
    Convert mebibytes (MiB) to bytes (binary).
    1 MiB = 1,048,576 bytes
    """
    return int(mib * MIB_TO_BYTES)


def parsemaxuntracked(opts) -> Optional[int]:
    if opts["max_untracked_size"] != "":
        return mib_to_bytes(float(opts["max_untracked_size"]))
    else:
        return None


def parsemaxuntrackedbytes(opts) -> Optional[int]:
    if opts["max_untracked_size_bytes"] != "":
        return int(opts["max_untracked_size_bytes"])
    else:
        return None


def parsemaxfilecount(opts):
    if opts["max_file_count"] != "":
        return opts["max_file_count"]
    else:
        return None


def parsecontinuationof(opts, repo):
    """Parse the continuation-of option to get the previous snapshot ID"""
    continuation_value = opts.get("continuation_of", "")
    if not continuation_value:
        return None

    if continuation_value == "latest":
        # Get the latest snapshot ID from metalog
        latest_snapshot = fetchlatestsnapshot(repo.metalog())
        if latest_snapshot is None:
            raise error.Abort(_("no latest snapshot found to continue from"))
        return latest_snapshot.hex()
    else:
        # Validate the hash format (should be 64 character hex string for bonsai hash)
        if len(continuation_value) != 64:
            raise error.Abort(
                _(
                    "invalid snapshot id format: expected 64 character hex string, got {}"
                ).format(len(continuation_value))
            )
        try:
            int(continuation_value, 16)
        except ValueError:
            raise error.Abort(
                _("invalid snapshot id format: '{}' is not a valid hex string").format(
                    continuation_value
                )
            )
        return continuation_value


def _fetchremotebubble(repo, cs_id_bytes):
    """Fetch bubble ID for a changeset from the remote server"""
    try:
        response = fetchsnapshot(repo, cs_id_bytes)
        bubble_id = response.get("bubble_id")
        if bubble_id is not None:
            return bubble_id
        else:
            return None

    except Exception as e:
        # Log the error but don't fail the operation
        repo.ui.debug(
            _("failed to fetch remote bubble for {}: {}\n").format(
                cs_id_bytes.hex(), str(e)
            )
        )
        return None


def parentsfromwctx(ui, wctx):
    p1 = wctx.p1().node()
    p2 = wctx.p2().node()
    if p2 != nullid and not ui.plain():
        ui.warn(
            _(
                "Conflict snapshots are not yet properly supported, "
                "but provided as best effort.\n"
            )
        )
    if p1 == nullid:
        return None
    return (p1, p2)


@dataclass(frozen=True)
class workingcopy:
    untracked: list
    removed: list
    modified: list
    added: list
    missing: list
    skipped_large_untracked: list

    def all(self):
        return self.untracked + self.removed + self.modified + self.added + self.missing

    def filecount(self):
        return (
            len(self.untracked)
            + len(self.removed)
            + len(self.modified)
            + len(self.added)
            + len(self.missing)
        )

    @staticmethod
    @perftrace.tracefunc("Create working copy")
    def fromrepo(repo, maxuntrackedsize, pats, opts):
        from sapling import scmutil

        wctx = repo[None]
        matcher = scmutil.match(wctx, pats, opts)
        skipped_large_files = []

        def filteruntracked(f):
            """Filter out non-regular files and files exceeding size limit."""
            if not repo.wvfs.isfileorlink(f):
                return False
            if maxuntrackedsize is not None:
                try:
                    size = repo.wvfs.lstat(f).st_size
                except OSError:
                    return False
                if size > maxuntrackedsize:
                    skipped_large_files.append(f)
                    if not repo.ui.plain():
                        repo.ui.warn(
                            _(
                                "not snapshotting '{}' because it is {} bytes large, and max untracked size is {} bytes\n"
                            ).format(f, size, maxuntrackedsize),
                            component="snapshot",
                        )
                    return False
            return True

        # Use single status call with matcher
        status = repo.status(match=matcher, unknown=True)

        untracked = [f for f in status.unknown if filteruntracked(f)]
        removed = []

        # Process removed files - check if they still exist (were hg rm'ed but recreated)
        for f in status.removed:
            if wctx[f].exists():
                if filteruntracked(f):
                    untracked.append(f)
            else:
                removed.append(f)

        return workingcopy(
            untracked=untracked,
            removed=removed,
            modified=list(status.modified),
            added=list(status.added),
            missing=list(status.deleted),
            skipped_large_untracked=skipped_large_files,
        )


@util.timefunction("snapshot_upload", 0, "ui")
def uploadsnapshot(repo, files_info, author, time, tz, hgparents, bubble_properties):
    return repo.edenapi.uploadsnapshot(
        {
            "files": files_info,
            "author": author,
            "time": int(time),
            "tz": tz,
            "hg_parents": hgparents[0],
        },
        bubble_properties,
    )


def createremote(ui, repo, *pats, **opts) -> None:
    reason = opts.get("reason")
    if reason:
        ui.log("snapshot_create_reason", snapshot_create_reason=reason)
    elif ui.interactive():
        ui.log("snapshot_create_reason", snapshot_create_reason="manual run")

    lifetime = _parselifetime(opts)
    maxuntrackedsize = parsemaxuntracked(opts)
    maxuntrackedsizebytes = parsemaxuntrackedbytes(opts)
    maxfilecount = parsemaxfilecount(opts)
    labels = parselabels(opts)
    continuationof = parsecontinuationof(opts, repo)
    keepttl = opts.get("keep_ttl", False)
    allowempty = ui.configbool("snapshot", "allowempty", True)

    # Validate keep-ttl is a boolean
    if not isinstance(keepttl, bool):
        raise error.Abort(
            _(
                "internal error: keep_ttl must be a boolean value, got {} of type {}"
            ).format(keepttl, type(keepttl).__name__)
        )

    # Validate keep-ttl flag: only allowed with continuation-of
    if keepttl and not continuationof:
        raise error.Abort(_("--keep-ttl flag can only be used with --continuation-of"))

    # Validate that lifetime is not specified with keep-ttl
    if keepttl and (lifetime is not None):
        raise error.Abort(
            _("--keep-ttl cannot be used with --lifetime (TTL extension is skipped)")
        )

    # Use bytes-based limit if specified, otherwise fall back to MiB-based limit, then config default
    effective_max_untracked_size = (
        maxuntrackedsizebytes or maxuntrackedsize or getdefaultmaxuntrackedsize(ui)
    )

    overrides = {}
    if ui.plain() or opts.get("template"):
        overrides[("ui", "quiet")] = True
    with ui.configoverride(overrides):
        with (
            repo.wlock(),
            repo.lock(),
        ):
            # Current working context
            wctx = repo[None]

            hgparents = parentsfromwctx(ui, wctx)
            if hgparents is None:
                raise error.Abort(_("snapshot creation requires working copy checkout"))

            # Get working copy state
            wc = workingcopy.fromrepo(repo, effective_max_untracked_size, pats, opts)
            filecount = wc.filecount()

            # Check for allowempty config and handle empty working copy
            if not allowempty and filecount == 0:
                parent_hex = hgparents[0].hex()

                # Handle JSON output if template is specified
                if opts.get("template"):
                    with ui.formatter("snapshot", opts) as fm:
                        fm.startitem()
                        fm.data(message="no changes")
                        fm.data(parent=parent_hex)
                        if not ui.quiet and not ui.plain():
                            fm.plain(
                                _("nothing to snapshot, parent commit is {}\n").format(
                                    parent_hex
                                )
                            )
                else:
                    ui.status(
                        _("nothing to snapshot, parent commit is {}\n").format(
                            parent_hex
                        ),
                        component="snapshot",
                    )
                return

            if filecount > maxfilecount:
                raise error.AbortSnapshotFileCountLimit(
                    _(
                        "snapshot file count limit exceeded: file count is {}, limit is {}"
                    ).format(filecount, maxfilecount)
                )

            (time, tz) = wctx.date()

            author = wctx.user()

            # Build file info while we have wlock
            files_info = {
                "root": repo.root,
                "modified": [(f, filetypefromfile(wctx[f])) for f in wc.modified],
                "added": [(f, filetypefromfile(wctx[f])) for f in wc.added],
                "untracked": [(f, filetypefromfile(wctx[f])) for f in wc.untracked],
                "removed": wc.removed,
                "missing": wc.missing,
            }

            # Look up bubble ID from the provided snapshot ID or latest snapshot
            previousbubble = (
                getcsidbubblemapping(repo.metalog(), continuationof)
                if continuationof
                else fetchlatestbubble(repo.metalog())
            )

        # Backup parents (network operation)
        _backupparents(repo, hgparents)

        # Handle continuation-of option
        bubble_strategy = "create_new"  # Default strategy, keep it explicit
        if continuationof:
            if previousbubble is None:
                # Try remote lookup for bubble ID if not found locally
                ui.status(
                    _("bubble not found locally for {}, trying remote lookup\n").format(
                        continuationof
                    ),
                    component="snapshot",
                )
                continuationof_bytes = bytes.fromhex(continuationof)
                previousbubble = _fetchremotebubble(repo, continuationof_bytes)

                if previousbubble is None:
                    # Still not found, raise an error
                    raise error.Abort(
                        _(
                            "cannot find bubble for previous snapshot {} (checked both local and remote)"
                        ).format(continuationof)
                    )
                else:
                    # Cache the remote result locally for future use
                    with repo.lock(), repo.transaction("snapshot_cache_bubble"):
                        metadata = SnapshotMetadata(
                            bubble=previousbubble, created_at=mtime.time()
                        )
                        storesnapshotmetadata(repo, continuationof_bytes, metadata)
                    ui.status(
                        _("found bubble for {} via remote lookup\n").format(
                            continuationof
                        ),
                        component="snapshot",
                    )

            # Reuse the bubble: extends TTL and reuses storage
            # When keep-ttl is set, skip TTL extension for performance and keep the existing TTL of the bubble
            # Client is responsible to ensure that the bubble is not expired:
            #
            # Use case example - chain of snapshots:
            #   1. Initial snapshot: Create first snapshot with --lifetime to set desired TTL
            #   2. Subsequent snapshots: Use --continuation-of <id> --keep-ttl for fast creation
            #      in quick succession without unnecessary TTL extension overhead

            bubble_strategy = {
                "reuse": {
                    "bubble_id": previousbubble,
                    "keep_ttl": keepttl,
                    "snapshot_id": continuationof,
                }
            }
            ui.status(
                _("continuing from snapshot {}\n").format(continuationof),
                component="snapshot",
            )
        elif previousbubble is not None:
            # We have a previous bubble known locally but not doing full continuation
            # Use it as a copy hint for performance optimization
            bubble_strategy = {"create_with_copy_hint": {"bubble_id": previousbubble}}

        # When keep-ttl is set, don't pass lifetime_secs (for performance: skip TTL extension)
        bubble_properties = {
            "lifetime_secs": None if keepttl else lifetime,
            "strategy": bubble_strategy,
            "labels": labels,
        }

        # Upload snapshot (network operation)
        response = uploadsnapshot(
            repo,
            files_info,
            author,
            time,
            tz,
            hgparents,
            bubble_properties,
        )

        csid = bytes(response["changeset_token"]["data"]["id"]["BonsaiChangesetId"])
        bubble = response["bubble_id"]
        bubble_expiration_timestamp = response.get("bubble_expiration_timestamp")
        snapshot_content_bytes = response.get("snapshot_content_bytes", 0)

        # Store metadata locally (requires lock, but brief)
        with repo.lock(), repo.transaction("snapshot"):
            storelatest(repo, csid, bubble, bubble_expiration_timestamp)

    csid = csid.hex()

    # Create file status summary
    status_parts = [
        _("{} files modified").format(len(wc.modified)),
        _("{} files added").format(len(wc.added)),
        _("{} files removed").format(len(wc.removed)),
        _("{} files missing").format(len(wc.missing)),
        _("{} files untracked").format(len(wc.untracked)),
    ]

    # Add skipped large files counter with bold formatting if any files were skipped
    if wc.skipped_large_untracked:
        skipped_text = _("{} large files skipped").format(
            len(wc.skipped_large_untracked)
        )
        status_parts.append(ui.label(skipped_text, "bold"))

    status_summary = ", ".join(status_parts)

    # Use formatter for JSON output support
    if opts.get("template"):
        with ui.formatter("snapshot", opts) as fm:
            fm.startitem()
            fm.data(id=csid)
            fm.data(bubble=bubble)
            # Add individual flat fields for template access
            fm.data(snapshot_modified=len(wc.modified))
            fm.data(snapshot_added=len(wc.added))
            fm.data(snapshot_removed=len(wc.removed))
            fm.data(snapshot_missing=len(wc.missing))
            fm.data(snapshot_untracked=len(wc.untracked))
            fm.data(snapshot_total_files=wc.filecount())
            fm.data(snapshot_content_bytes=snapshot_content_bytes)
            fm.data(parent=hgparents[0].hex())
            if labels:
                fm.data(labels=labels)

            # Add skipped large untracked files to JSON output
            if wc.skipped_large_untracked:
                fm.data(skipped_large_untracked=wc.skipped_large_untracked)

            # Add bubble expiration timestamp to JSON output
            # The ephemeral storage unit is not specific to this only snapshot,
            # its lifetime could be extended but never shortened
            # The expiration timestamp is only used for the purpose of hinting the remaining lifetime to the customer
            if bubble_expiration_timestamp is not None:
                fm.data(storage_expiration_timestamp=bubble_expiration_timestamp)

            # For non-JSON output, provide the traditional format
            if not ui.quiet:
                if ui.plain():
                    fm.plain(f"{csid}\n")
                elif labels:
                    labels_str = ",".join(labels)
                    fm.plain(
                        _(
                            "snapshot: Snapshot created with id {} and labels {}\n{}\n"
                        ).format(csid, labels_str, status_summary)
                    )
                else:
                    fm.plain(
                        _("snapshot: Snapshot created with id {}\n{}\n").format(
                            csid, status_summary
                        )
                    )
    else:
        if ui.plain():
            ui.status(f"{csid}\n")
        elif labels:
            labels = ",".join(labels)
            ui.status(
                _("Snapshot created with id {} and labels {}\n{}\n").format(
                    csid, labels, status_summary
                ),
                component="snapshot",
            )
        else:
            ui.status(
                _("Snapshot created with id {}\n{}\n").format(csid, status_summary),
                component="snapshot",
            )
