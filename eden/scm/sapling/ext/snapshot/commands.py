# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from sapling import cmdutil, error, registrar
from sapling.i18n import _

from . import createremote, isworkingcopy, labels, latest, list, show, update

cmdtable = {}
command = registrar.command(cmdtable)


@command("snapshot", [], "SUBCOMMAND ...")
def snapshot(ui, repo, **opts):
    """create and share snapshots with uncommitted changes"""

    raise error.Abort(
        "you need to specify a subcommand (run with --help to see a list of subcommands)"
    )


subcmd = snapshot.subcommand(
    categories=[
        ("Manage snapshots", ["create", "update", "add-labels", "remove-labels"]),
        ("Query snapshots", ["show", "list"]),
    ]
)


@subcmd(
    "createremote|create",
    [
        (
            "L",
            "lifetime",
            "",
            _(
                "how long the snapshot should last for, seconds to days supported (e.g. 60s, 90d, 1h30m)"
            ),
            _("LIFETIME"),
        ),
        (
            "",
            "labels",
            "",
            _(
                "comma-separated list of named labels to be associated with the snapshot. Named snapshots will not expire"
            ),
            _("LABELS"),
        ),
        (
            "",
            "max-file-count",
            1000,
            _("maximum allowed total number of files in a snapshot"),
            _("MAX_FILE_COUNT"),
        ),
        (
            "",
            "continuation-of",
            "",
            _(
                "snapshot id of previous snapshot to continue from; "
                "reusing the same ephemeral bubble and extending its TTL; "
                "'latest' is an alias for the hash of the last created snapshot in the checkout"
            ),
            _("HASH"),
        ),
        (
            "",
            "keep-ttl",
            False,
            _(
                "skip TTL extension when using --continuation-of for performance; "
                "only valid with --continuation-of flag"
            ),
        ),
        (
            "",
            "reason",
            "",
            _("specify the reason for creating this snapshot for logging purposes"),
            _("REASON"),
        ),
    ]
    + cmdutil.untrackedfileopts
    + cmdutil.walkopts
    + cmdutil.templateopts,
)
def createremotecmd(*args, **kwargs) -> None:
    """upload to the server a snapshot of the current uncommitted changes

    The --continuation-of option allows you to continue from a previous
    snapshot, reusing the same ephemeral storage and extending its TTL. This is
    useful for creating a series of related snapshots that share the same
    storage. Note that Source Control ephemeral storage is internally called "ephemeral bubble".

    Lifetime Management:
      Extends the TTL of ephemeral storage when using '--continuation-of'.
      The lifetime remains unchanged if the duration specified via --lifetime
        is shorter than the current remaining lifetime.
      If no --lifetime is provided, the lifetime resets to the greater of the
        default lifetime or the remaining lifetime.
      Returns an error if the storage has expired.

    Exits with code 2 if the file count in the snapshot will exceed max-file-
    count.
    """
    createremote.createremote(*args, **kwargs)


@subcmd(
    "update|restore|checkout|co|up|goto",
    [
        (
            "C",
            "clean",
            None,
            _("discard uncommitted changes and untracked files (no backup)"),
        )
    ],
    _("ID"),
)
def updatecmd(ui, repo, csid=None, **opts) -> None:
    """download a previously created snapshot and update working copy to its state"""
    if csid is None:
        raise error.CommandError("snapshot update", _("missing snapshot id"))
    update.update(ui, repo, csid, **opts)


@subcmd(
    "show|info",
    [
        ("", "json", None, _("output in json format instead of human-readable")),
        ("", "stat", None, _("output diffstat-style summary of changes")),
    ],
    _("ID"),
)
def showcmd(*args, **kwargs) -> None:
    """gather information about the snapshot"""
    show.show(*args, **kwargs)


@subcmd(
    "isworkingcopy",
    cmdutil.untrackedfileopts + cmdutil.walkopts + cmdutil.templateopts,
    _("ID"),
)
def isworkingcopycmd(*args, **kwargs) -> None:
    """test if a given snapshot is the working copy

    This command compares the current working copy state against a previously
    created snapshot to determine if they match.

    Include/Exclude Pattern Behavior:
      When using -I (include) or -X (exclude) patterns, the comparison applies
      the same filtering to BOTH the working copy and the snapshot files.

      For example:
        hg snapshot isworkingcopy SNAPSHOT_ID -X '*.tmp'

      This ignores all .tmp files when comparing, regardless of whether the
      original snapshot included .tmp files or not. The patterns control
      which files are considered for the comparison, not which files the
      snapshot should have contained.

    This allows you to check working copy equivalence while ignoring certain
    file types (e.g., temporary files, build artifacts) that may have been
    present when the snapshot was created.
    """
    isworkingcopy.cmd(*args, **kwargs)


@subcmd(
    "latest",
    cmdutil.templateopts,
)
def latestcmd(*args, **kwargs) -> None:
    """information regarding the latest created snapshot

    Restored snapshot is not marked at "latest" until the next snapshot is created.
    """
    latest.latest(*args, **kwargs)


@subcmd(
    "add-labels",
    [
        (
            "",
            "labels",
            "",
            _("comma-separated list of named labels to be added to the snapshot"),
        ),
    ],
    _("ID"),
)
def add_labels(*args, **kwargs) -> None:
    """Associate new labels with an existing snapshot"""
    labels.add_labels(*args, **kwargs)


@subcmd(
    "remove-labels",
    [
        (
            "",
            "labels",
            "",
            _(
                "comma-separated list of named labels to be removed from the snapshot. Cannot be used with --all"
            ),
        ),
        (
            "",
            "all",
            False,
            _(
                "flag representing if all the labels associated with the snapshot need to be removed. Cannot be used with --labels"
            ),
        ),
    ],
    _("ID"),
)
def remove_labels(*args, **kwargs) -> None:
    """Remove associated labels from an existing snapshot"""
    labels.remove_labels(*args, **kwargs)


@subcmd(
    "list",
    [
        (
            "l",
            "limit",
            "",
            _("limit the number of snapshots to show"),
            _("NUM"),
        ),
        (
            "s",
            "since",
            "",
            _("show snapshots created since this date/time"),
            _("DATE"),
        ),
    ]
    + cmdutil.templateopts,
)
def listcmd(*args, **kwargs) -> None:
    """list locally known snapshots"""
    list.list_snapshots(*args, **kwargs)
