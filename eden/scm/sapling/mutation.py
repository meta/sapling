# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# mutation.py - commit mutation tracking


from collections import defaultdict
from typing import Dict, List, Optional

from bindings import mutationstore

from . import error, node as nodemod, perftrace, phases, util
from .node import nullid


def identfromnode(node) -> str:
    return "hg/%s" % nodemod.hex(node)


def nodefromident(ident):
    if ident.startswith("hg/"):
        return nodemod.bin(ident[3:])
    raise error.Abort("Unrecognised commit identifier: %s" % ident)


def record(repo, extra, prednodes, op=None, splitting=None) -> Optional[Dict[str, str]]:
    mutinfo = None
    for key in "mutpred", "mutuser", "mutdate", "mutop", "mutsplit":
        if key in extra:
            del extra[key]
    if enabled(repo):
        mutinfo = {}
        mutinfo["mutpred"] = ",".join(identfromnode(p) for p in prednodes)
        mutinfo["mutuser"] = repo.ui.config("mutation", "user") or repo.ui.username()
        date = repo.ui.config("devel", "default-date")
        if date is None:
            date = util.makedate()
        else:
            date = util.parsedate(date)
        mutinfo["mutdate"] = "%d %d" % date
        if op is not None:
            mutinfo["mutop"] = op
        if splitting:
            mutinfo["mutsplit"] = ",".join(identfromnode(n) for n in splitting)
        if recording(repo):
            extra.update(mutinfo)
    return mutinfo


def recording(repo):
    return repo.ui.configbool("mutation", "record")


def enabled(repo):
    return repo.ui.configbool("mutation", "enabled")


def allow_divergence(ui):
    """Whether operations may deliberately create divergent (obsolete) commits.

    ``experimental.evolution.allowdivergence`` is the pre-mutation name, still
    honored for callers that set it by hand.
    """
    return ui.configbool("mutation", "allow-divergence") or ui.configbool(
        "experimental", "evolution.allowdivergence"
    )


def makemutationstore(repo):
    return mutationstore.mutationstore(repo.svfs.join("mutation"))


class bundlemutationstore:
    def __init__(self, bundlerepo):
        self._entries = {}
        self._splitheads = {}
        self._successorssets = {}
        self._mutationstore = makemutationstore(bundlerepo)

    def addbundleentries(self, entries):
        for entry in entries:
            self._entries[entry.succ()] = entry
            succs = []
            for split in entry.split() or ():
                self._splitheads[split] = entry.succ()
                succs.append(split)
            succs.append(entry.succ())
            for pred in entry.preds():
                self._successorssets.setdefault(pred, []).append(succs)

    def get(self, node):
        return self._mutationstore.get(node) or self._entries.get(node)

    def getsplithead(self, node):
        return self._mutationstore.getsplithead(node) or self._splitheads.get(node)

    def getsuccessorssets(self, node):
        storesets = self._mutationstore.getsuccessorssets(node)
        bundlesets = self._successorssets.get(node, [])
        if bundlesets and storesets:
            return util.removeduplicates(storesets + bundlesets)
        else:
            return storesets or bundlesets

    def has(self, node):
        return self._mutationstore.has(node) or node in self._entries

    def add(self, entry):
        # bundlerepo mutation stores are immutable.  This should never be called.
        raise NotImplementedError

    def flush(self):
        pass


def nodesfrominfo(info):
    if info is not None:
        return [nodefromident(x) for x in info.split(",")]


def createentry(node, mutinfo):
    if mutinfo is not None:
        try:
            time, tz = mutinfo["mutdate"].split()
            time = int(time)
            tz = int(tz)
        except (IndexError, ValueError):
            time, tz = 0, 0
        return mutationstore.mutationentry(
            node,
            nodesfrominfo(mutinfo.get("mutpred")),
            nodesfrominfo(mutinfo.get("mutsplit")),
            mutinfo.get("mutop", ""),
            mutinfo.get("mutuser", ""),
            time,
            tz,
            None,
        )


def createsyntheticentry(
    repo, preds, succ, op, splitting=None, user=None, date=None, extras=None
):
    user = user or repo.ui.config("mutation", "user") or repo.ui.username()
    date = date or repo.ui.config("devel", "default-date")
    if date is None:
        date = util.makedate()
    else:
        date = util.parsedate(date)
    return mutationstore.mutationentry(
        succ, preds, splitting, op, user, int(date[0]), int(date[1]), extras
    )


def recordentries(repo, entries, skipexisting: bool = True, raw: bool = False) -> int:
    count = 0
    with repo.transaction("record-mutation") as tr:
        ms = repo._mutationstore
        if raw:
            add = ms.addraw
        else:
            add = ms.add
        tr.addfinalize("mutation", lambda _tr: ms.flush())
        for entry in entries:
            if skipexisting and ms.has(entry.succ()):
                continue
            add(entry)
            count += 1
    return count


def getdag(
    repo,
    *nodes,
    predecessors: bool = True,
    successors: bool = True,
    splits: bool = False,
):
    """Get the mutation subgraph for selected nodes

    If successors is True, follow successors.
    If predecessors is True, follow predecessors.
    A fold's commit has every folded commit as a parent. If splits is True,
    every part of a split has the split commit as a parent; otherwise only
    the last part does.

    Performance note: predecessors might be too many if a commit
    is changed many times. In some cases it's possible to only
    use successors and that might be faster.
    """
    return repo._mutationstore.getdag(
        nodes, predecessors=predecessors, successors=successors, splits=splits
    )


def historynodes(repo, nodes):
    """The commits available locally in the mutation history of ``nodes``.

    That is ``nodes``, the visible commits connected to them by mutations in
    either direction (including other folded commits and other parts of a
    split), and every commit those were created from. A hidden commit that
    only leads to hidden commits is left out, unless ``--hidden`` makes every
    commit visible.
    """
    cl = repo.changelog
    dag = getdag(repo, *nodes, splits=True)
    isvisible = getisvisiblefunc(repo)
    visible = [n for n in cl.filternodes(list(dag.all()), local=True) if isvisible(n)]
    return cl.filternodes(list(dag.ancestors(visible + list(nodes))), local=True)


def lookup(repo, node):
    """Look up mutation information for the given node"""
    return repo._mutationstore.get(node)


def lookupsplit(repo, node):
    """Look up mutation information for the given node, or the main split node
    if this node is the result of a split.
    """
    ms = repo._mutationstore
    mainnode = ms.getsplithead(node) or node
    return ms.get(mainnode)


def lookupsuccessors(repo, node):
    """Look up the immediate successors sets for the given node"""
    succsets = sorted(repo._mutationstore.getsuccessorssets(node))
    return util.removesortedduplicates(succsets)


def allpredecessors(repo, nodes, startdepth=None, stopdepth=None):
    """Yields all the nodes that are predecessors of the given nodes.

    Some predecessors may not be known locally."""
    depth = 0
    thislevel = set(nodes)
    nextlevel = set()
    seen = {nullid}
    ispublic = getispublicfunc(repo)
    islocal = getislocal(repo)
    while thislevel and (stopdepth is None or depth < stopdepth):
        for current in thislevel:
            if current in seen:
                continue
            seen.add(current)
            if startdepth is None or depth >= startdepth:
                yield current
            pred = None
            entry = lookupsplit(repo, current)
            if entry is not None:
                pred = entry.preds()
            if pred is not None:
                for nextnode in pred:
                    if nextnode not in seen and (
                        not islocal(nextnode) or not ispublic(nextnode)
                    ):
                        nextlevel.add(nextnode)
        depth += 1
        thislevel = nextlevel
        nextlevel = set()


def allsuccessors(repo, nodes, startdepth=None, stopdepth=None):
    """Yields all the nodes that are successors of the given nodes.

    Successors that are not known locally may be omitted."""
    depth = 0
    thislevel = set(nodes)
    nextlevel = set()
    seen = set()
    ispublic = getispublicfunc(repo)
    islocal = getislocal(repo)
    while thislevel and (stopdepth is None or depth < stopdepth):
        for current in thislevel:
            if current in seen:
                continue
            seen.add(current)
            if startdepth is None or depth >= startdepth:
                yield current
            if islocal(current) and ispublic(current):
                continue
            succsets = lookupsuccessors(repo, current)
            if succsets:
                nextlevel = nextlevel.union(*succsets)
        depth += 1
        thislevel = nextlevel
        nextlevel = set()


class obsoletecache:
    def __init__(self):
        # Set of commits that are known to be obsolete for each filter level.
        self.obsolete = defaultdict(set)

        # Set of commits that are known to be not obsolete for each filter level.
        self.notobsolete = defaultdict(set)

        # If true, then the full set of obsolete commits is known for this
        # filter level, and is stored in ``self.obsolete``.
        self.complete = defaultdict(bool)

    def isobsolete(self, repo, node):
        """Returns true if the node is obsolete in the repository."""
        if node is None:
            return False
        if self.complete[None]:
            # The full set covers every non-public node in the repo, so
            # membership alone answers the question.
            return node in self.obsolete[None]
        if node not in repo:
            return False
        ispublic = getispublicfunc(repo)
        if ispublic(node):
            return False
        obsolete = self.obsolete[None]
        if node in obsolete:
            return True
        if node in self.notobsolete[None]:
            return False
        clhasnode = getisvisiblefunc(repo)

        for succ in allsuccessors(repo, [node], startdepth=1):
            # If any successor is already known to be obsolete, we can
            # assume that the current node is obsolete without checking further.
            if succ in obsolete:
                obsolete.add(node)
                return True
            # The node is obsolete if any successor is visible in the normal
            # filtered repo.
            if clhasnode(succ):
                obsolete.add(node)
                return True
        self.notobsolete[None].add(node)
        return False

    def obsoletenodes(self, repo):
        if self.complete[None]:
            return self.obsolete[None]

        with perftrace.trace("Compute Obsolete Nodes"):
            perftrace.traceflag("mutation")

            cl = repo.changelog
            torevs = getattr(cl, "torevs", None)
            # Use native path if modern DAG API is available.
            if torevs is not None:
                getphase = repo._phasecache.getrevset
                tonodes = cl.tonodes
                draftnodes = tonodes(getphase(repo, [phases.draft]))
                publicnodes = tonodes(getphase(repo, [phases.public]))
                ms = repo._mutationstore
                obsolete = ms.calculateobsolete(publicnodes, draftnodes)
                self.obsolete[None] = obsolete
                self.complete[None] = True
                return obsolete

            # Testing each node separately will result in lots of repeated tests.
            # Instead, we can do the following:
            # - Compute all nodes that are obsolete because one of their closest
            #   successors is visible.
            # - Work back from these commits marking all of their predecessors as
            #   obsolete.
            # Note that "visible" here means "visible in a normal filtered repo",
            # even if the filter for this repo includes other commits.
            clhasnode = getisvisiblefunc(repo)
            obsolete = self.obsolete[None]
            for node in repo.nodes("not public()"):
                if any(
                    clhasnode(succ)
                    for succ in allsuccessors(repo, [node], startdepth=1)
                ):
                    obsolete.add(node)
            candidates = set(obsolete)
            seen = set(obsolete)
            while candidates:
                candidate = candidates.pop()
                entry = lookupsplit(repo, candidate)
                if entry:
                    for pred in entry.preds():
                        if pred not in obsolete and pred not in seen:
                            candidates.add(pred)
                            seen.add(pred)
                            if clhasnode(pred) and pred != nullid:
                                obsolete.add(pred)
            self.obsolete[None] = frozenset(obsolete)
            self.complete[None] = True
            # Since we know all obsolete commits, no need to remember which ones
            # are not obsolete.
            if None in self.notobsolete:
                del self.notobsolete[None]
            return self.obsolete[None]


def isobsolete(repo, node):
    if not hasattr(repo, "_mutationobsolete"):
        repo._mutationobsolete = obsoletecache()
    return repo._mutationobsolete.isobsolete(repo, node)


def obsoletenodes(repo):
    if not hasattr(repo, "_mutationobsolete"):
        repo._mutationobsolete = obsoletecache()
    return repo._mutationobsolete.obsoletenodes(repo)


def clearobsoletecache(repo) -> None:
    if hasattr(repo, "_mutationobsolete"):
        del repo._mutationobsolete


def fate(repo, node):
    """Returns the fate of a node.

    This returns a list of ([nodes], operation) pairs, indicating mutations that
    happened to this node that resulted in one or more visible commits.
    """
    ispublic = getispublicfunc(repo)
    fate = []
    if isobsolete(repo, node):
        for succset in successorssets(repo, node, closest=True):
            if succset == [node]:
                pass
            elif len(succset) > 1:
                fate.append((succset, "split"))
            else:
                succ = succset[0]
                # Base the default operation name on the successor's phase
                if ispublic(succ):
                    op = "land"
                else:
                    op = "rewrite"
                # Try to find the real operation name.
                entry = lookup(repo, succ)
                if entry is not None:
                    preds = entry.preds()
                    if preds is not None and node in preds:
                        op = entry.op() or op
                fate.append((succset, op))
    return fate


def predecessorsset(repo, startnode, closest: bool = False):
    """Return a list of the commits that were replaced by the startnode.

    If there are no such commits, returns a list containing the startnode.

    If ``closest`` is True, returns a list of the visible commits that are the
    closest previous version of the start node.

    If ``closest`` is False, returns a list of the earliest original versions of
    the start node.
    """
    seen = {startnode}
    ispublic = getispublicfunc(repo)

    def get(node):
        """Get immediate predecessors

        Returns a list of immediate predecessors of the node, omitting
        any predecessors which have already been seen, to prevent loops.

        If the node has no predecessors, returns a list containing the node
        itself.
        """
        entry = lookupsplit(repo, node)
        if entry is not None:
            preds = entry.preds()
            if preds is not None:
                return [
                    pred for pred in preds if pred not in seen and not ispublic(pred)
                ] or [node]
        return [node]

    preds = [startnode]
    nextpreds = sum((get(p) for p in preds), [])
    expanded = nextpreds != preds
    while expanded:
        seen.update(nextpreds)
        if all(p in repo for p in nextpreds):
            # We have found a set of predecessors that are all visible - this is
            # a valid set to return.
            preds = nextpreds
            if closest:
                break
            # Now look at the next predecessors of each commit.
            newnextpreds = sum((get(p) for p in nextpreds), [])
        else:
            # Expand out to the predecessors of the commits until we find visible
            # ones.
            newnextpreds = sum(([p] if p in repo else get(p) for p in nextpreds), [])
        expanded = newnextpreds != nextpreds
        nextpreds = newnextpreds
        if not expanded:
            # We've reached a stable state and some of the commits might not be
            # visible.  Remove the invisible commits, and continue with what's
            # left.
            newnextpreds = [p for p in nextpreds if p in repo]
            if newnextpreds:
                expanded = newnextpreds != nextpreds
                nextpreds = newnextpreds
        # Remove duplicates from the list of predecessors.  Splits and folds
        # can lead to the same commits via multiple routes, but we don't need
        # to process them multiple times.
        nextpreds = util.removeduplicates(nextpreds)
    return preds


def local_closest_predecessors(repo, node) -> List[bytes]:
    entry = repo._mutationstore.get(node)
    preds = entry and entry.preds() or []
    # Filter preds by "locally known"
    if preds:
        preds = repo.changelog.filternodes(preds, local=True)
    return list(preds)


def _succproduct(succsetlist):
    """Takes a list of successorsset lists and returns a single successorsset
    list representing the cartesian product of those successorsset lists.

    The ordering of elements within the lists must be preserved.

    >>> _succproduct([[[1]], [[2]]])
    [[1, 2]]
    >>> _succproduct([[[1, 2]], [[3, 4]]])
    [[1, 2, 3, 4]]
    >>> _succproduct([[[1, 2], [3, 4]], [[5, 6]]])
    [[1, 2, 5, 6], [3, 4, 5, 6]]
    >>> _succproduct([[[1, 2], [3, 4]], [[5, 6], [7, 8]]])
    [[1, 2, 5, 6], [3, 4, 5, 6], [1, 2, 7, 8], [3, 4, 7, 8]]
    >>> _succproduct([[[1, 2], [3, 4]], [[2, 3], [7, 8]]])
    [[1, 2, 3], [3, 4, 2], [1, 2, 7, 8], [3, 4, 7, 8]]
    >>> _succproduct([[[1, 2], [3, 4]], [[1, 2], [7, 8]]])
    [[1, 2], [3, 4, 1, 2], [1, 2, 7, 8], [3, 4, 7, 8]]
    >>> _succproduct([[[1], [2]], [[3], [4]]])
    [[1, 3], [2, 3], [1, 4], [2, 4]]
    >>> _succproduct([[[5]], [[4]], [[3]], [[2]], [[1]]])
    [[5, 4, 3, 2, 1]]
    """
    # Start with the first successorsset.
    product = succsetlist[0]
    for succsets in succsetlist[1:]:
        # For each of the remaining successorssets, compute the product with
        # the successorsset so far.
        newproduct = []
        for succset in succsets:
            for p in product:
                newproduct.append(p + [s for s in succset if s not in p])
        product = newproduct
    return product


def successorssets(repo, startnode, closest: bool = False, cache=None):
    """Return a list of lists of commits that replace the startnode.

    If there are no such commits, returns a list containing a single list
    containing the startnode.

    If ``closest`` is True, the lists contain the visible commits that are the
    closest next version of the start node.

    If ``closest`` is False, the lists contain the latest versions of the start
    node.

    The ``cache`` parameter is unused.  It is provided to make this function
    signature-compatible with ``obsutil.successorssets``.
    """
    seen = {startnode}
    ispublic = getispublicfunc(repo)
    islocal = getislocal(repo)

    def getsets(node):
        """Get immediate successors sets

        Returns a list of immediate successors sets of the node, omitting
        any sets which contain nodes already seen, to prevent loops.

        If the node has no successors, returns a list containing a single
        successors set which contains the node itself.
        """
        if islocal(node) and ispublic(node):
            return [[node]]
        succsets = [
            succset
            for succset in lookupsuccessors(repo, node)
            if not any(succ in seen for succ in succset)
        ]
        return succsets or [[node]]

    clhasnode = getisvisiblefunc(repo)

    succsets = [[startnode]]
    nextsuccsets = getsets(startnode)
    expanded = nextsuccsets != succsets
    while expanded:
        seen.update(node for succset in nextsuccsets for node in succset)
        if all(clhasnode(s) for succset in nextsuccsets for s in succset):
            # We have found a set of successor sets that all contain visible
            # commits - this is a valid set to return.
            succsets = nextsuccsets
            if closest:
                break

            # Now look at the next successors of each successors set.  When
            # commits are modified in different ways (i.e. they have been
            # diverged), we need to find all possible permutations that replace
            # the original nodes.  To do this, we compute the cartesian product
            # of the successors sets of each successor in the original
            # successors set.
            #
            # For example, if A is split into B and C, B is diverged to B1 and
            # B2, and C is also diverged to C1 and C2, then the successors sets
            # of A are: [B1, C1], [B1, C2], [B2, C1], [B2, C2], which is the
            # cartesian product: [B1, B2] x [C1, C2].
            newnextsuccsets = sum(
                [
                    _succproduct([getsets(succ) for succ in succset])
                    for succset in nextsuccsets
                ],
                [],
            )
        else:
            # Expand each successors set out to its successors until we find
            # visible commit.  Again, use the cartesian product to find all
            # permutations.
            newnextsuccsets = sum(
                [
                    _succproduct(
                        [
                            [[succ]] if clhasnode(succ) else getsets(succ)
                            for succ in succset
                        ]
                    )
                    for succset in nextsuccsets
                ],
                [],
            )
        expanded = newnextsuccsets != nextsuccsets
        nextsuccsets = newnextsuccsets
        if not expanded:
            # We've reached a stable state and some of the commits might not be
            # visible.  Remove the invisible commits, and continue with what's
            # left.
            newnextsuccsets = [
                [s for s in succset if clhasnode(s)] for succset in nextsuccsets
            ]
            # Remove sets that are now empty.
            newnextsuccsets = [succset for succset in newnextsuccsets if succset]
            if newnextsuccsets:
                expanded = newnextsuccsets != nextsuccsets
                nextsuccsets = newnextsuccsets
        # Remove duplicates from the list of successors sets.  Splits and folds
        # can lead to the same commits via multiple routes, but we don't need
        # to process them multiple times.
        nextsuccsets = util.removeduplicates(nextsuccsets, key=frozenset)
    return succsets


def foreground_contains(repo, old_nodes, new_node):
    """Test if `ancestors(new_node)` overlap with `old_nodes`.

    This is not quite as the same as the original 'foreground', but is much
    faster since we avoid passing a large set to `predecessors` or
    `successors` (or, a large `predecessors` set to `ancestors`).
    """
    # First, check without calculating `predecessors`.
    ancestor_nodes = repo.dageval(lambda: ancestors([new_node]))
    if any(n in ancestor_nodes for n in old_nodes):
        return True
    # For performance we don't check `predecessors` at all.
    return False


def unbundle(repo, bundledata) -> None:
    if enabled(repo):
        entries = mutationstore.unbundle(bundledata)
        recordentries(repo, entries, skipexisting=True, raw=True)


def entriesfornodes(repo, nodes):
    """Generate mutation entries for the given nodes"""
    nodes = set(nodes)
    remaining = set(nodes)
    seen = set()
    entries = []
    while remaining:
        current = remaining.pop()
        if current in seen:
            continue
        seen.add(current)

        entry = lookupsplit(repo, current)
        if entry is not None:
            entries.append(entry)
            for nextnode in entry.preds():
                if nextnode not in seen:
                    remaining.add(nextnode)

    return entries


def bundleentries(entries):
    return mutationstore.bundle(entries)


def bundle(repo, nodes):
    """Generate bundled mutation data for bundling alongside the given nodes."""
    entries = entriesfornodes(repo, nodes)
    return bundleentries(entries)


def getislocal(repo):
    """get a (node) -> bool function to test if node is known locally"""
    filternodes = repo.changelog.filternodes

    def islocal(
        node,
        filternodes=filternodes,
    ):
        return bool(filternodes([node], local=True))

    return islocal


def getisvisiblefunc(repo):
    """get a (node) -> bool function to test visibility

    If narrow-heads is set, visible commits are defined as "::head()".

    visibility here is used to test where a "successor" exists or not.
    """
    if repo.ui.configbool("experimental", "narrow-heads"):
        # Use phase to test.
        # secret phase means "not reachable from public or draft heads", aka. "hidden"
        nodemap = repo.changelog.nodemap
        getphase = repo._phasecache.phase
        islocal = getislocal(repo)

        def isvisible(
            node,
            nodemap=nodemap,
            islocal=islocal,
            getphase=getphase,
            repo=repo,
            secret=phases.secret,
        ):
            if not islocal(node):
                # Does not exist in the local graph (local=True avoids asking the server)
                return False
            try:
                # This might trigger remote lookup.
                rev = nodemap[node]
            except error.RevlogError:
                return False
            return getphase(repo, rev) != secret

        return isvisible
    else:
        # Use cl.hasnode to test
        return repo.changelog.hasnode


def getispublicfunc(repo):
    """get a (node) -> bool function to test whether a commit is public"""
    nodemap = repo.changelog.nodemap
    getphase = repo._phasecache.phase

    def ispublic(
        node, nodemap=nodemap, getphase=getphase, repo=repo, public=phases.public
    ):
        try:
            rev = nodemap[node]
        except error.RevlogError:
            return False
        return getphase(repo, rev) == public

    return ispublic
