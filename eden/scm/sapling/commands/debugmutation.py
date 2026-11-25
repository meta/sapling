# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# debugmutation.py - command processing for debug commands for mutation and visibility


from .. import error, json, mutation, node as nodemod, scmutil, util, visibility
from ..i18n import _, _x
from .cmdtable import command


@command(
    "debugmutation",
    [
        ("r", "rev", [], _("display predecessors of REV")),
        ("s", "successors", False, _("show successors instead of predecessors")),
        ("t", "time-range", [], _("select time range"), _("TIME")),
        ("T", "template", "", _("display with template"), _("TEMPLATE")),
    ],
    cmdtype=command.readonly,
)
def debugmutation(ui, repo, **opts) -> int:
    """display the mutation history (or future) of a commit

    ``-Tjson`` outputs the predecessor mutations for one target. The output
    contains raw mutation facts and does not attest that the history is complete.
    """
    unfi = repo

    template = opts.get("template")
    if template and template != "json":
        raise error.Abort(_("debugmutation only supports -Tjson"))

    if template == "json":
        if opts.get("successors"):
            raise error.Abort(_("-Tjson does not support --successors"))
        if opts.get("time_range"):
            raise error.Abort(_("-Tjson does not support --time-range"))

        revs = scmutil.revrange(repo, opts.get("rev") or ["."])
        if len(revs) != 1:
            raise error.Abort(_("-Tjson requires exactly one revision"))

        target = repo[revs.first()].node()
        remaining = [target]
        seen = set()
        emitted_primary = set()
        entries = []
        while remaining:
            current = remaining.pop()
            if current in seen:
                continue
            seen.add(current)

            entry = mutation.lookupsplit(unfi, current)
            if entry is None:
                continue

            primary = entry.succ()
            if primary in emitted_primary:
                continue
            emitted_primary.add(primary)
            successor = nodemod.hex(primary)
            split_successors = {nodemod.hex(node) for node in entry.split()}
            if split_successors:
                split_successors.add(successor)
            predecessors = sorted({nodemod.hex(node) for node in entry.preds()})
            entries.append(
                {
                    "successor": successor,
                    "predecessors": predecessors,
                    "split_successors": sorted(split_successors),
                    "operation": entry.op(),
                }
            )
            remaining.extend(entry.preds())

        entries.sort(key=lambda entry: entry["successor"])
        ui.write(
            "%s\n"
            % json.dumps(
                {
                    "target": nodemod.hex(target),
                    "mutations": entries,
                }
            )
        )
        return 0

    matchdatefuncs = []
    for timerange in opts.get("time_range") or []:
        matchdate = util.matchdate(timerange)
        matchdatefuncs.append(matchdate)

    def dateinrange(timestamp):
        return not matchdatefuncs or any(m(timestamp) for m in matchdatefuncs)

    def describe(entry, showsplit=False, showfoldwith=None):
        mutop = entry.op()
        mutuser = entry.user()
        mutdate = util.shortdatetime((entry.time(), entry.tz()))
        mutpreds = entry.preds()
        mutsplit = entry.split() or None
        extra = ""
        if showsplit and mutsplit is not None:
            extra += " (split into this and: %s)" % ", ".join(
                [nodemod.hex(n) for n in mutsplit]
            )
        if showfoldwith is not None:
            foldwith = [pred for pred in mutpreds if pred != showfoldwith]
            if foldwith:
                extra += " (folded with: %s)" % ", ".join(
                    [nodemod.hex(n) for n in foldwith]
                )
        return "%s by %s at %s%s" % (mutop, mutuser, mutdate, extra)

    def expandhistory(node):
        entry = mutation.lookup(unfi, node)
        if entry is None:
            return []
        if not dateinrange(entry.time()):
            return [("...", [])]
        desc = describe(entry, showsplit=True) + " from:"
        preds = util.removeduplicates(entry.preds())
        return [(desc, preds)]

    def expandfuture(node):
        succsets = mutation.lookupsuccessors(unfi, node)
        edges = []
        for succset in succsets:
            entry = mutation.lookupsplit(unfi, succset[0])
            if dateinrange(entry.time()):
                desc = describe(entry, showfoldwith=node) + " into:"
                edges.append((desc, succset))
            else:
                edges.append(("...", []))
        return edges

    expand = expandfuture if opts.get("successors") else expandhistory

    def rendernodes(prefix, nodes):
        if len(nodes) == 1:
            render(prefix, prefix, nodes[0])
        elif len(nodes) > 1:
            for node in nodes[:-1]:
                render(prefix + "|-  ", prefix + "|   ", node)
            render(prefix + "'-  ", prefix + "    ", nodes[-1])

    def render(firstprefix, prefix, nextnode):
        while nextnode:
            node = nextnode
            nextnode = None
            ui.status(_x("%s%s") % (firstprefix, nodemod.hex(node)))
            firstprefix = prefix
            edges = expand(node)
            if len(edges) == 0:
                ui.status(_x("\n"))
            elif len(edges) == 1:
                desc, nextnodes = edges[0]
                ui.status(_x(" %s\n") % desc)
                if len(nextnodes) == 1:
                    # Simple case, don't use recursion
                    nextnode = nextnodes[0]
                else:
                    rendernodes(prefix, nextnodes)
            elif len(edges) > 1:
                ui.status(_x(" diverges\n"))
                for edge in edges[:-1]:
                    desc, nextnodes = edge
                    ui.status(_x("%s:=  %s\n") % (prefix, desc))
                    rendernodes(prefix + ":   ", nextnodes)
                desc, nextnodes = edges[-1]
                ui.status(_x("%s'=  %s\n") % (prefix, desc))
                rendernodes(prefix + "    ", nextnodes)

    for rev in scmutil.revrange(repo, opts.get("rev") or ["."]):
        render(" *  ", "    ", repo[rev].node())
        ui.status(_x("\n"))

    return 0


@command("debugmutationfromobsmarkers", [])
def debugmutationfromobsmarkers(ui, repo, **opts) -> int:
    """convert obsolescence markers to mutation records"""
    # pyre-fixme[16]: Module `mutation` has no attribute `convertfromobsmarkers`.
    entries, commits, written = mutation.convertfromobsmarkers(repo)
    repo.ui.write(
        _("wrote %s of %s entries for %s commits\n") % (written, entries, commits)
    )
    return 0


@command("debugvisibility", [], subonly=True)
def debugvisibility(ui, repo) -> None:
    """control visibility tracking"""


subcmd = debugvisibility.subcommand()


@subcmd("start", [])
def debugvisibilitystart(ui, repo) -> int:
    """start tracking commit visibility explicitly"""
    visibility.starttracking(repo)
    return 0


@subcmd("stop", [])
def debugvisibilitystop(ui, repo) -> int:
    """stop tracking commit visibility explicitly"""
    visibility.stoptracking(repo)
    return 0


@subcmd("status", [])
def debugvisibilitystatus(ui, repo) -> None:
    """show current visibility tracking status"""
    if visibility.enabled(repo):
        ui.status(_("commit visibility is tracked explicitly\n"))
    else:
        ui.status(_("commit visibility is not tracked\n"))
