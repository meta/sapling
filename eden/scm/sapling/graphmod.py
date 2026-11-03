# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Revision graph generator for Mercurial
#
# Copyright 2008 Dirkjan Ochtman <dirkjan@ochtman.nl>
# Copyright 2007 Joel Rosdahl <joel@rosdahl.net>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

"""supports walking the history as DAGs suitable for graphical output

The most basic format we use is that of::

  (id, type, data, [parentids])

The node and parent ids are arbitrary integers which identify a node in the
context of the graph returned. Type is a constant specifying the node type.
Data depends on type.
"""

from . import dagop, mutation, smartset
from .node import nullid

CHANGESET = "C"
# A commit that is not available locally; its data is its node.
MISSINGCHANGESET = "MC"
PARENT = "P"
GRANDPARENT = "G"
XREPOPARENT = "X"
MISSINGPARENT = "M"
# Style of line to draw. None signals a line that ends and is removed at this
# point. A number prefix means only the last N characters of the current block
# will use that style, the rest will use the PARENT style. Add a - sign
# (so making N negative) and all but the first N characters use that style.
EDGES = {PARENT: "|", GRANDPARENT: ":", MISSINGPARENT: None}


def dagwalker(repo, revs, template, idfunc=None, dag=None):
    """cset DAG generator yielding (id, CHANGESET, ctx, [parentinfo]) tuples

    This generator function walks through revisions (which should be ordered
    from bigger to lower). It returns a tuple for each node.

    Each parentinfo entry is a tuple with (edgetype, parentid), where edgetype
    is one of PARENT, GRANDPARENT or MISSINGPARENT. The node and parent ids
    are arbitrary integers which identify a node in the context of the graph
    returned.

    The idfunc is a function that takes a rev number and returns an id for it.

    If dag is set, use the specified dag instead.
    """
    if not revs:
        return

    simplifygrandparents = repo.ui.configbool("log", "simplify-grandparents")
    cl = repo.changelog
    if cl.algorithmbackend != "segments":
        simplifygrandparents = False
    rootnodes = cl.tonodes(revs)
    if dag is None:
        dag = cl.dag
    else:
        # more efficient when running `& rootnodes` below.
        rootnodes = dag.sort(rootnodes)

    if idfunc is None:
        idfunc = lambda rev: rev

    gpcache = {}
    ctxstream = revs.prefetchbytemplate(repo, template).iterctx()
    for ctx in ctxstream:
        # partition into parents in the rev set and missing parents, then
        # augment the lists with markers, to inform graph drawing code about
        # what kind of edge to draw between nodes.
        node = ctx.node()
        if node == nullid:
            parent_nodes = []
        else:
            parent_nodes = dag.parentnames(node)
        pset_nodes = {n for n in parent_nodes if n in rootnodes}
        pset = set(cl.torevs(pset_nodes))
        mpars_nodes = [n for n in parent_nodes if n not in pset_nodes]
        mpars = [cl.rev(n) for n in mpars_nodes]
        parents = [(PARENT, idfunc(p)) for p in sorted(pset)]

        for mpar in mpars:
            gp = gpcache.get(mpar)
            if gp is None:
                if simplifygrandparents:
                    gp = gpcache[mpar] = cl.torevs(
                        dag.headsancestors(dag.ancestors([cl.node(mpar)]) & rootnodes)
                    )

                else:
                    # precompute slow query as we know reachableroots() goes
                    # through all revs (issue4782)
                    if not isinstance(revs, smartset.baseset):
                        revs = smartset.baseset(revs, repo=repo)
                    gp = gpcache[mpar] = sorted(
                        set(dagop.reachableroots(repo, revs, [mpar]))
                    )
            if not gp:
                parents.append((MISSINGPARENT, idfunc(mpar)))
                pset.add(mpar)
            else:
                parents.extend((GRANDPARENT, idfunc(g)) for g in gp if g not in pset)
                pset.update(gp)

        yield (idfunc(ctx.rev()), CHANGESET, ctx, parents)


def mutationdagwalker(repo, revs, template, showmissing=False):
    """mutation DAG generator yielding (node, type, data, [parentinfo])

    Like ``dagwalker``, but a commit's parents are the commits it replaced
    (the predecessor of an amend or rebase, every commit of a fold, the
    commit a split part came from), and nodes and parent ids are commit
    hashes. ``revs`` are yielded as ``CHANGESET``s with their ctx, in the
    order of ``mutationsort``.

    A predecessor that is shown is a ``PARENT``. One that is not is skipped:
    the nearest shown commits it was created from are ``GRANDPARENT``s, or
    if there are none it is a ``MISSINGPARENT``. With ``showmissing``, the
    commits ``revs`` were created from that are not available locally are
    shown too, as ``MISSINGCHANGESET``s whose data is the node.
    """
    order, parents, missing = _mutationgraph(repo, revs, showmissing)
    cl = repo.changelog
    local = smartset.baseset([cl.rev(n) for n in order if n not in missing], repo=repo)
    ctxs = {
        ctx.node(): ctx for ctx in local.prefetchbytemplate(repo, template).iterctx()
    }
    for node in order:
        if node in missing:
            yield (node, MISSINGCHANGESET, node, parents[node])
        else:
            yield (node, CHANGESET, ctxs[node], parents[node])


def mutationsort(repo, revs):
    """``revs`` as a list, each commit before the commits it was created from.

    A chain of mutations is kept together; otherwise newer revs come first.
    """
    order, _parents, _missing = _mutationgraph(repo, revs)
    cl = repo.changelog
    return [cl.rev(n) for n in order]


def _mutationgraph(repo, revs, showmissing=False):
    """Return (nodes, parents, missing) for showing ``revs`` as a mutation
    graph.

    ``nodes`` are the commits to show, sorted as ``mutationsort`` describes:
    ``revs`` and, with ``showmissing``, the commits they were created from
    that are not available locally, which are also returned as ``missing``.
    ``parents`` maps each node to its parentinfo list.
    """
    cl = repo.changelog
    nodes = list(cl.tonodes(revs))
    dag = mutation.getdag(repo, *nodes, splits=True)
    missing = set()
    if showmissing and nodes:
        predecessors = list(dag.ancestors(nodes))
        local = set(cl.filternodes(predecessors, local=True))
        missing = {n for n in predecessors if n not in local}
        nodes += sorted(missing)
    shownset = dag.sort(nodes)
    shown = set(nodes)

    parents = {}
    for node in nodes:
        edges = []
        seen = set()
        for pred in dag.parentnames(node):
            if pred in shown:
                candidates = [(PARENT, pred)]
            else:
                nearest = dag.headsancestors(dag.ancestors([pred]) & shownset)
                candidates = [(GRANDPARENT, n) for n in nearest] or [
                    (MISSINGPARENT, pred)
                ]
            for edge in candidates:
                if edge[1] not in seen:
                    seen.add(edge[1])
                    edges.append(edge)
        parents[node] = edges

    # Topological sort, successors first. Picking the most recently readied
    # commit keeps a chain of mutations together; otherwise the newer rev
    # (commits not available locally have no rev, and count as oldest).
    children = {node: 0 for node in nodes}
    for edges in parents.values():
        for kind, pred in edges:
            if kind != MISSINGPARENT:
                children[pred] += 1

    def revof(node):
        return -1 if node in missing else cl.rev(node)

    ready = sorted((n for n in nodes if children[n] == 0), key=revof)
    order = []
    while ready:
        node = ready.pop()
        order.append(node)
        newlyready = []
        for kind, pred in parents[node]:
            if kind != MISSINGPARENT:
                children[pred] -= 1
                if children[pred] == 0:
                    newlyready.append(pred)
        ready.extend(sorted(newlyready, key=revof))
    return order, parents, missing
