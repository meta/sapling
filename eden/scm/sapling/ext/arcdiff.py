# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# arcdiff.py - extension adding an option to the diff command to show changes
#              since the last arcanist diff

import os

from sapling import (
    autopull,
    commands,
    error,
    extensions,
    mdiff,
    patch,
    registrar,
    revset,
    scmutil,
    smartset,
)
from sapling.i18n import _
from sapling.node import hex

from .extlib.phabricator import arcconfig, diffprops, graphql

hint = registrar.hint()
revsetpredicate = registrar.revsetpredicate()


def extsetup(ui) -> None:
    entry = extensions.wrapcommand(commands.table, "diff", _diff)
    options = entry[1]
    options.append(
        ("", "since-last-submit", None, _("show changes since last Phabricator submit"))
    )
    options.append(
        (
            "",
            "since-last-submit-2o",
            None,
            _("show diff of current diff and last Phabricator submit"),
        )
    )


def _differentialhash(ui, repo, phabrev):
    timeout = repo.ui.configint("ssl", "timeout", 10)
    signalstatus = repo.ui.configbool("ssl", "signal_status", True)
    try:
        client = graphql.Client(repodir=repo.root, repo=repo)
        info = client.getrevisioninfo(timeout, signalstatus, [phabrev]).get(
            str(phabrev)
        )
        if not info:
            return None
        return info

    except graphql.ClientError as e:
        ui.warn(_("Error calling graphql: %s\n") % str(e))
        return None
    except arcconfig.ArcConfigError as e:
        raise error.Abort(str(e))


def _diff2o(ui, repo, rev1, rev2, *pats, **opts) -> None:
    # Phabricator revs are often filtered (hidden)
    # First reconstruct textual diffs for rev1 and rev2 independently.
    def changediff(node):
        ctx = repo[node]
        basectx = ctx.p1()
        m = scmutil.matchall(repo)
        diff = patch.diffhunks(repo, basectx, ctx, m, opts=mdiff.diffopts(context=0))
        filepatches = {}
        for _fctx1, _fctx2, headerlines, hunks in diff:
            difflines = []
            for hunkrange, hunklines in hunks:
                difflines += list(hunklines)
            header = patch.header(headerlines)
            filepatches[header.files()[0]] = b"".join(difflines)
        return (set(filepatches.keys()), filepatches, node)

    rev1node = repo[rev1].node()
    rev2node = repo[rev2].node()
    files1, filepatches1, node1 = changediff(rev1node)
    files2, filepatches2, node2 = changediff(rev2node)

    (ui.write(_("Phabricator rev: %s\n") % hex(node1)),)
    ui.write(_("Local rev: %s (%s)\n") % (hex(node2), rev2))

    # For files have changed, produce a diff of the diffs first using a normal
    # text diff of the input diffs, then fixing-up the output for readability.
    changedfiles = files1 & files2
    for f in changedfiles:
        opts["context"] = 0
        diffopts = mdiff.diffopts(**opts)
        header, hunks = mdiff.unidiff(
            filepatches1[f], "", filepatches2[f], "", f, f, opts=diffopts
        )
        hunklines = []
        for hunk in hunks:
            hunklines += hunk[1]
        changelines = []
        i = 0
        while i < len(hunklines):
            line = hunklines[i].decode(errors="replace")
            i += 1
            if line[:2] == "++":
                changelines.append("+" + line[2:])
            elif line[:2] == "+-":
                changelines.append("-" + line[2:])
            elif line[:2] == "-+":
                changelines.append("-" + line[2:])
            elif line[:2] == "--":
                changelines.append("+" + line[2:])
            elif line[:2] == "@@" or line[1:3] == "@@":
                if len(changelines) < 1 or changelines[-1] != "...\n":
                    changelines.append("...\n")
            else:
                changelines.append(line)
        if len(changelines):
            ui.write(_("Changed: %s\n") % f)
            for line in changelines:
                ui.write("| " + line)
    wholefilechanges = files1 ^ files2
    for f in wholefilechanges:
        ui.write(_("Added/removed: %s\n") % f)


def _maybepull(repo, hexrev) -> None:
    autopull.trypull(repo, [hexrev])


def _diff(orig, ui, repo, *pats, **opts):
    if not opts.get("since_last_submit") and not opts.get("since_last_submit_2o"):
        return orig(ui, repo, *pats, **opts)

    if len(opts["rev"]) > 1:
        mess = _("cannot specify --since-last-submit with multiple revisions")
        raise error.Abort(mess)
    try:
        targetrev = opts["rev"][0]
    except IndexError:
        targetrev = "."
    ctx = repo[targetrev]
    phabrev = diffprops.parserevfromcommitmsg(ctx.description())

    if phabrev is None:
        mess = _("local changeset is not associated with a differential revision")
        hint = (
            _("it has no diff yet: '@prog@ diff -c %s' shows all its changes")
            % targetrev
        )
        raise error.Abort(mess, hint=hint)

    rev = _differentialhash(ui, repo, phabrev)

    if rev is None or not isinstance(rev, dict) or "hash" not in rev:
        mess = _("unable to determine previous changeset hash")
        raise error.Abort(mess)

    rev = str(rev["hash"])
    _maybepull(repo, rev)
    opts["rev"] = [rev, targetrev]

    # if patterns aren't provided, restrict diff to files in both changesets
    # this prevents performing a diff on rebased changes
    if len(pats) == 0:
        prev = set(repo[rev].files())
        curr = set(repo[targetrev].files())
        pats = tuple(os.path.join(repo.root, p) for p in prev | curr)

    if opts.get("since_last_submit_2o"):
        return _diff2o(ui, repo, rev, targetrev, **opts)
    else:
        return orig(ui, repo, *pats, **opts)


@revsetpredicate("lastsubmitted(set)")
def lastsubmitted(repo, subset, x):
    revs = revset.getset(repo, revset.fullreposet(repo), x)
    phabrevs = set()
    for rev in revs:
        phabrev = diffprops.parserevfromcommitmsg(repo[rev].description())
        if phabrev is None:
            mess = _("local changeset is not associated with a differential revision")
            raise error.Abort(mess)
        phabrevs.add(phabrev)

    resultrevs = set()
    for phabrev in phabrevs:
        diffrev = _differentialhash(repo.ui, repo, phabrev)
        if diffrev is None or not isinstance(diffrev, dict) or "hash" not in diffrev:
            mess = _("unable to determine previous changeset hash")
            raise error.Abort(mess)

        lasthash = str(diffrev["hash"])
        _maybepull(repo, lasthash)
        resultrevs.add(repo[lasthash].rev())

    return subset & smartset.baseset(sorted(resultrevs), repo=repo)
