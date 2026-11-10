# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from typing import List, Tuple

from sapling import error, util
from sapling.i18n import _

from .metalog import readmetadatas


def list_snapshots(ui, repo, **opts) -> None:
    """
    List locally known snapshots from metalog cache.

    This lists snapshots created on this host as a flat list ordered by time.
    Since, on the backend, we do not have a capability to list snapshots by client yet,
    we only list snapshots locally known to this checkout from the metalog cache.
    Only snapshots that have been created or restored on this checkout are shown.

            - limit: Maximum number of snapshots to show
            - since: Show snapshots created since this time
    """
    ml = repo.metalog()
    metadatas = readmetadatas(ml)
    snapshots_dict = metadatas.get("snapshots", {})

    # Parse since filter if provided
    since_timestamp = 0.0  # Keep as float to match created_at type
    since_str = opts.get("since")
    if since_str and since_str.strip():  # Check for non-empty string
        try:
            # util.parsedate returns int, but we need float for comparison
            since_timestamp = float(util.parsedate(since_str)[0])
        except error.ParseError:
            raise error.Abort(_("invalid date format: {}").format(since_str))

    # Convert to list of tuples (cs_id, metadata) and filter by since
    snapshot_items: List[Tuple[str, dict]] = []
    for cs_id, metadata in snapshots_dict.items():
        created_at = metadata.get("created_at")
        if created_at is None:
            raise error.Abort(
                _("snapshot {} missing created_at metadata").format(cs_id)
            )
        # Both created_at and since_timestamp are now floats
        if created_at > since_timestamp:
            snapshot_items.append((cs_id, metadata))

    # Sort by creation time (most recent first)
    snapshot_items.sort(key=lambda x: x[1]["created_at"], reverse=True)

    # Apply limit if specified
    limit = opts.get("limit")
    if limit is not None:
        if isinstance(limit, str):
            limit = limit.strip()
            if limit == "":
                limit = None
            else:
                try:
                    limit = int(limit)
                except ValueError:
                    raise error.Abort(
                        _("invalid limit value: {}").format(opts.get("limit"))
                    )

        if limit is not None:
            if limit <= 0:
                raise error.Abort(_("limit must be a positive integer"))
            snapshot_items = snapshot_items[:limit]

    # Use formatter for consistent output
    ui.pager("snapshot list")
    with ui.formatter("snapshot", opts) as fm:
        if not snapshot_items:
            if fm.isplain():
                ui.status(_("no snapshots found\n"))
            return

        # Print header for human-readable output
        if fm.isplain() and snapshot_items:
            fm.plain(
                _(
                    "| Snapshot ID                                                      | Creation Time                  | Expiration Time                |\n"
                ),
            )
            fm.plain(
                _(
                    "|------------------------------------------------------------------|--------------------------------|--------------------------------|\n"
                ),
            )

        for cs_id, metadata in snapshot_items:
            cs_id = cs_id.hex()
            fm.startitem()
            fm.data(id=cs_id)
            fm.data(created_at=metadata.get("created_at"))
            fm.data(bubble=metadata.get("bubble"))
            fm.data(
                storage_expiration_timestamp=metadata.get("bubble_expiration_timestamp")
            )

            if fm.isplain():
                # Human-readable output
                created_at = metadata.get("created_at")
                expiration_timestamp = metadata.get("bubble_expiration_timestamp")

                # Validate required metadata
                if created_at is None:
                    raise error.Abort(
                        _("snapshot {} missing created_at metadata").format(cs_id)
                    )

                # Format timestamp using Sapling's datestr
                created_str = util.datestr((created_at, 0))

                # Format expiration timestamp if available
                # Expiration time is per ephemeral bubble, so it could be extended but not shortened
                if expiration_timestamp is not None:
                    expiration_str = util.datestr((expiration_timestamp, 0))
                else:
                    # If not available, use "Unknown Locally" for now, later we could fetch from remote
                    expiration_str = "Unknown Locally"

                # Display full snapshot ID (no truncation) with proper table formatting
                fm.write("id", "| %-64s", cs_id)
                fm.write("created_at", " | %-30s", created_str)
                fm.write("bubble_expiration_timestamp", " | %-30s", expiration_str)
                fm.plain(" |\n")
            else:
                # JSON/template output - data is already set above
                pass
