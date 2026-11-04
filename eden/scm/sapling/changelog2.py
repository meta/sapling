# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.


import time
import weakref
from functools import partial
from typing import IO, Optional, TYPE_CHECKING, Union

import bindings

from . import (
    bookmarks as bookmod,
    changelog as changelogmod,
    error,
    git,
    mdiff,
    progress,
    revlog,
    smartset,
    util,
    vfs as vfsmod,
    visibility,
)
from .changelog import changelogrevision2, gitcommittext, hgcommittext
from .i18n import _
from .node import bin, hex, nullid, nullrev, wdirid, wdirrev

if TYPE_CHECKING:
    from .signing import SigningBackend

SEGMENTS_DIR = "segments/v1"
SEGMENTS_DIR_NEXT = "segments/v1next"  # Used on Windows, for migration.
HGCOMMITS_DIR = "hgcommits/v1"


class changelog:
    """Changelog backed by Rust objects.

    Many methods exist for compatibility. New code should consider using `dag`,
    `dageval`, `changelogrevision` for reads, avoiding other read operations.
    """

    _delayed = True
    indexfile = "00changelog.i"
    storedeltachains = False

    def __init__(self, repo, inner, uiconfig):
        """Construct changelog backed by Rust objects."""
        svfs = repo.svfs
        self.svfs = svfs
        self.inner = inner
        self._uiconfig = uiconfig
        self._isgit = git.isgitformat(repo)
        # Number of commit texts to buffer. Useful for bounding memory usage.
        self._groupbuffersize = uiconfig.configint("pull", "buffer-commit-count")
        self._reporef = weakref.ref(repo)
        format = self._isgit and "git" or "hg"
        self._changelogrevision_ctor = partial(changelogrevision2, format=format)

        # Cache for node->changeset
        self._cs_cache = util.lrucachedict(100)

    @util.propertycache
    def _visibleheads(self):
        # TODO: Consider moving visibleheads out?
        return self._loadvisibleheads(self.svfs)

    @property
    def algorithmbackend(self):
        return self.inner.algorithmbackend()

    def isvertexlazy(self):
        return self.dag.isvertexlazy()

    @property
    def opener(self):
        return self.svfs

    @classmethod
    def openrevlog(cls, repo, uiconfig):
        """Construct changelog from 00changelog.i revlog."""
        svfs = repo.svfs
        inner = bindings.dag.commits.openrevlog(svfs.join(""))
        return cls(repo, inner, uiconfig)

    @classmethod
    def opensegments(cls, repo, uiconfig):
        svfs = repo.svfs
        segmentsdir = _segmentsdir(svfs)
        hgcommitsdir = svfs.join(HGCOMMITS_DIR)
        inner = bindings.dag.commits.opensegments(segmentsdir, hgcommitsdir)
        return cls(repo, inner, uiconfig)

    @property
    def dag(self):
        """Get the DAG with algorithms. Require rust-commit."""
        return self.inner.dag()

    def dageval(self, func, extraenv=None):
        """Evaluate func with the current DAG context.

        Example:

            cl.dageval(lambda: roots(ancestors([x])))

            # avoid conflict with local variable named 'only'
            cl.dageval(lambda dag: dag.only(x, y))

        is equivalent to:

            dag = cl.dag
            dag.roots(dag.ancestors([x]))

        'extraenv' is optionally a dict. It sets extra names. For example,
        providing revset-like functions like `draft`, `public`, `bookmarks`.
        """
        env = dict(func.__globals__)
        dag = self.dag
        for name in func.__code__.co_names:
            # name is potentially a string used by LOAD_GLOBAL bytecode.
            if extraenv is not None and name in extraenv:
                # Provided by 'extraenv'.
                env[name] = extraenv[name]
            else:
                # Provided by 'dag'.
                value = getattr(dag, name, None)
                if value is not None:
                    env[name] = value
        code = func.__code__
        argdefs = func.__defaults__
        closure = func.__closure__
        name = func.__name__
        # Create a new function that uses the same logic except for different
        # env (globals).
        newfunc = type(func)(code, env, name, argdefs, closure)
        if code.co_argcount == 1 and not argdefs:
            return newfunc(dag)
        else:
            return newfunc()

    @property
    def idmap(self):
        """Get the IdMap. Require rust-commit."""
        return self.inner.idmap()

    @property
    def hasnode(self):
        """Test if a node is in the graph.
        Can trigger remote request if isvertexlazy() is True.
        Use filternodes to batch the testing.
        """
        return self.idmap.__contains__

    def filternodes(self, nodes, inverse=False, local=False):
        """Take a list of nodes, return a list of nodes present in the graph.
        This will only send one remote request. Therefore more efficient than
        "hasnode".

        If inverse is True, return nodes not present in the graph instead.
        If local is True, avoid asking remote servers (for a lazy changelog).
        """
        return self.idmap.filternodes(nodes, inverse=inverse, local=local)

    @property
    def torevs(self):
        """Convert a Set using commit hashes to an IdSet using numbers

        Does not preserve laziness of the set. Can be slow on large sets,
        unless they are backed by efficient Rust structures.

        The Set is usually obtained via `self.dag` APIs.
        """
        return self.inner.torevs

    def torevset(self, nodes, reverse=False):
        """Convert to a smartset.nameset for revset compatibility.

        Preserve laziness of the set.

        `nodes` should be obtained via `self.dag` APIs.

        New code should avoid this and use the native node-based interface
        instead.

        The Rust set uses DESC order by default. Setting `reverse` to True
        will reverse the order.
        """
        return smartset.nameset(nodes, reverse=reverse, repo=self._reporef())

    def tonodes(self, revs, preserve_order=False):
        """Convert an IdSet to Set. The reverse of torevs."""
        # translate fullreposet to dag.all() that preserves the 'full' hint.
        if isinstance(revs, smartset.fullreposet):
            return self.dag.all()
        # 'nameset' has a fast path - it contains the Rust nameset that uses
        # nodes directly.
        if isinstance(revs, smartset.nameset):
            return revs._set
        return self.inner.tonodes(revs, preserve_order=preserve_order)

    def _loadvisibleheads(self, svfs):
        return visibility.visibleheads(svfs)

    def tip(self) -> bytes:
        tip = self.svfs.tryread("tip")
        if tip and self.hasnode(tip):
            return tip
        return self.dag.all().first() or nullid

    def __contains__(self, rev):
        """filtered version of revlog.__contains__"""
        if rev is None:
            return False
        # The Rust set only takes i64 revision numbers; no revision lies outside.
        if isinstance(rev, int) and not -(1 << 63) <= rev < (1 << 63):
            return False
        return rev in self.torevs(self.dag.all())

    def __iter__(self):
        """filtered version of revlog.__iter__"""
        return self.torevs(self.dag.all()).iterasc()

    def __len__(self):
        return len(self.dag.all())

    def revs(self, start=0, stop=None):
        """filtered version of revlog.revs"""
        allrevs = self.torevs(self.dag.all())
        if stop is not None:
            # exclusive -> inclusive
            stop = stop - 1
        revs = bindings.dag.spans.unsaferange(start, stop) & allrevs
        for i in revs.iterasc():
            yield i

    @property
    def nodemap(self):
        # self.idmap might change, do not cache this nodemap result
        return nodemap(self)

    def reachableroots(self, minroot, heads, roots, includepath=False):
        tonodes = self.tonodes
        headnodes = tonodes(heads)
        rootnodes = tonodes(roots)
        dag = self.dag
        if includepath:
            # special case: null::X -> ::X
            if len(rootnodes) == 0 and nullrev in roots:
                nodes = dag.ancestors(headnodes)
            else:
                nodes = dag.range(rootnodes, headnodes)
        else:
            nodes = dag.reachableroots(rootnodes, headnodes)
        return list(self.torevs(nodes))

    def rawheadrevs(self):
        """Raw heads that exist in the changelog.
        This bypasses the visibility layer.

        This can be expensive and should be avoided if possible.
        """
        dag = self.dag
        heads = dag.headsancestors(dag.all())
        # Be compatible with C index headrevs: Return in ASC order.
        revs = self.torevs(heads)
        return list(revs.iterasc())

    def strip(self, minlink, transaction):
        # This should only be used in tests that uses 'debugstrip'
        if util.istest():
            raise error.Abort(_("strip is not supported"))
        # Invalidate on-disk nodemap.
        if self.indexfile.startswith("00changelog"):
            self.svfs.tryunlink("00changelog.nodemap")
            self.svfs.tryunlink("00changelog.i.nodemap")
        self.inner.strip([self.node(minlink)])

    @util.recordtracebacks()
    def rev(self, node):
        if node == wdirid:
            raise error.WdirUnsupported
        try:
            return self.idmap.node2id(node)
        except error.CommitLookupError:
            raise error.LookupError(node, self.indexfile, _("no node"))

    @util.recordtracebacks()
    def node(self, rev):
        if rev == wdirrev:
            raise error.WdirUnsupported
        try:
            return self.idmap.id2node(rev)
        except error.CommitLookupError:
            raise IndexError("revlog index out of range")

    def linkrev(self, rev):
        return rev

    def flags(self, rev):
        return 0

    def delayupdate(self, tr):
        pass

    def read(self, node):
        """Obtain data from a parsed changelog revision.

        Returns a 6-tuple of:

           - manifest node in binary
           - author/user as a localstr
           - date as a 2-tuple of (time, timezone)
           - list of files
           - commit message as a localstr
           - dict of extra metadata

        Unless you need to access all fields, consider calling
        ``changelogrevision`` instead, as it is faster for partial object
        access.
        """
        c = self._changelogrevision_ctor(self.revision(node))
        return (c.manifest, c.user, c.date, c.files, c.description, c.extra)

    def changelogrevision(self, nodeorrev):
        """Obtain a ``changelogrevision`` for a node or revision."""

        # Be sure to cache by node, not rev. Revision numbers can be re-assigned.
        if isinstance(nodeorrev, bytes):
            node = nodeorrev
        else:
            node = self.node(nodeorrev)

        cs = self._cs_cache.get(node, None)
        if cs is not None:
            return cs

        cs = self._changelogrevision_ctor(self.revision(node))
        self._cs_cache[node] = cs
        return cs

    def readfiles(self, node):
        """
        short version of read that only returns the files modified by the cset
        """
        # For performance we skip verifying the commit hash, which can trigger
        # remote lookups of the parent hashes. A real-world example is:
        # log -r " reverse(master~1000::master) & not(file(r're:.*'))"
        c = self._changelogrevision_ctor(self.revision(node, verify=False))
        return c.files

    def add(
        self,
        manifest,
        files,
        desc,
        transaction,
        p1,
        p2,
        user,
        date=None,
        extra=None,
        signing_backend: "Optional[SigningBackend]" = None,
    ):
        parents = [p for p in (p1, p2) if p != nullid]
        if self._isgit:
            # 'files' is not used by git
            text = gitcommittext(
                manifest,
                parents,
                desc,
                user,
                date,
                extra,
                signing_backend=signing_backend,
            )
            node = git.hashobj(b"commit", text)
        else:
            text = hgcommittext(
                manifest,
                files,
                desc,
                user,
                date,
                extra,
            )
            node = revlog.hash(text, p1, p2)

        # Avoid updating "tip" if node is known locally.
        # Strictly speaking this should check with the remote server for lazy
        # changelog. Practically that breaks offline committing and the server
        # check is almost always "not found", and "tip" does not matter that
        # much.
        if not self.filternodes([node], local=True):
            self.svfs.write("tip", node)

        self.inner.addcommits([(node, parents, text)])
        nodes = transaction.changes.get("nodes")
        if nodes is not None:
            nodes.append(node)
        return node

    def addgroup(self, deltas, linkmapper, transaction):
        nodes = []
        textmap = {}  # {node: btext}
        commits = []
        buffersize = self._groupbuffersize
        tip = None
        for node, p1, p2, linknode, deltabase, delta, flags in deltas:
            assert flags == 0, "changelog flags cannot be non-zero"
            parents = [p for p in (p1, p2) if p != nullid]
            basetext = textmap.get(deltabase) or self.revision(deltabase)
            rawtext = bytes(mdiff.patch(basetext, delta))
            if b"stepparents:" in rawtext:
                parents += parse_stepparents(
                    self._changelogrevision_ctor(rawtext).extra
                )
            textmap[node] = rawtext
            commits.append((node, parents, rawtext))
            # Attempt to make memory usage bound for large pulls.
            if len(commits) > buffersize:
                # PERF: Calling addcommits here is suboptimal for segments
                # backend, because its commit graph bookkeeping has overhead,
                # and we don't need such bookkeeping (ex. commits are querable
                # in the graph).
                # Ideally there is a "addcommitsdata" API to only add the
                # commit data (text). However, revlog won't support such API
                # so it might be a good idea to consider it after switching
                # away from revlog.
                self.inner.addcommits(commits)
                # Flush the commit data (major memory consumption).
                #
                # Flushing the commit graph is tricky for segmented changelog,
                # as we need to know the location of "master" here, which is
                # a difficult problem.
                self.inner.flushcommitdata()
                commits[:] = []
            nodes.append(node)
        if nodes:
            tip = nodes[-1]
        # Call 'addcommits' once with batched commits is important for
        # performance.
        self.inner.addcommits(commits)
        trnodes = transaction.changes.get("nodes")
        if trnodes is not None:
            trnodes += nodes
        if tip is not None:
            self.svfs.write("tip", tip)
        return nodes

    def branchinfo(self, rev):
        """return the branch name and open/close state of a revision

        This function exists because creating a changectx object
        just to access this is costly."""
        extra = self.read(rev)[5]
        return extra.get("branch"), "close" in extra

    def revision(
        self,
        nodeorrev: "Union[int, bytes]",
        _df: "Optional[IO]" = None,
        raw: bool = False,
        verify: bool = True,
    ) -> bytes:
        if nodeorrev in {nullid, nullrev}:
            return b""
        if isinstance(nodeorrev, bytes):
            node = nodeorrev
        else:
            node = self.node(nodeorrev)

        text = self.inner.getcommitrawtext(node)
        if text is None:
            raise error.LookupError(node, self.indexfile, _("no node"))

        # Do not verify hg hash if git hash is being used.
        if verify and not self._isgit:
            # check HG SHA1 hash
            p1, p2 = self.parents(node)[:2]
            if not self._should_skip_hash_check and revlog.hash(text, p1, p2) != node:
                if (
                    "emergencychangelog" in self._reporef().storerequirements
                    and self.rev(node) == 0
                ):
                    # The first node in an emergencychangelog repo has the
                    # wrong parents.
                    return text
                raise error.RevlogError(
                    _("integrity check failed on commit %s") % hex(node)
                )

        return text

    @util.propertycache
    def _should_skip_hash_check(self):
        repo = self._reporef()
        return "invalid-hash" in repo.storerequirements

    def nodesbetween(self, roots, heads):
        """Calculate (roots::heads, roots & (roots::heads), heads & (roots::heads))"""
        result = self.dag.range(roots, heads)
        roots = roots & result
        heads = heads & result
        # Return in ASC order to be compatible with the old logic.
        return list(result.iterrev()), list(roots.iterrev()), list(heads.iterrev())

    def children(self, node):
        """Return children(node)"""
        children = self.dag.children([node])
        children_except_wdir = [n for n in children if n != wdirid]
        return children_except_wdir

    def descendants(self, revs):
        """Return ((revs::) - roots(revs)) in revs."""
        dag = self.dag
        # nullrev special case.
        if nullrev in revs:
            result = dag.all()
        else:
            nodes = self.tonodes(revs)
            result = dag.descendants(nodes) - dag.roots(nodes) - dag.virtualgroup()
        for rev in self.torevs(result).iterasc():
            yield rev

    def findcommonmissing(self, common, heads):
        """Return (torevset(::common), (::heads) - (::common))"""
        # "::heads - ::common" is "heads % common", aka. the "only"
        # operation.
        onlyheads, commonancestors = self.dag.onlyboth(heads, common)
        # commonancestors can be lazy (revlog backend), use torevset instead of
        # torevs.
        return self.torevset(commonancestors), list(onlyheads.iterrev())

    def findmissing(self, common, heads):
        """Return 'heads % common'"""
        return list(self.dag.only(heads, common).iterrev())

    def findmissingrevs(self, common, heads):
        tonodes = self.tonodes
        torevs = self.torevs
        return torevs(self.findmissing(tonodes(common), tonodes(heads)))

    def isancestor(self, a, b):
        """Test if a (in node) is an ancestor of b (in node)"""
        if a == nullid or b == nullid:
            return False
        return self.dag.isancestor(a, b)

    def ancestor(self, a, b):
        """Return the common ancestor, or nullid if there are no common
        ancestors.

        Common ancestors are defined as heads(::a & ::b).

        When there are multiple common ancestors, a "random" one is returned.
        """
        if nullid == a or nullid == b:
            return nullid
        return self.dag.gcaone([a, b]) or nullid

    def descendant(self, start, end):
        """Test if start (in rev) is an ancestor of end (in rev)"""
        return self.isancestor(self.node(start), self.node(end))

    def _partialmatch(self, hexprefix):
        matched = self.idmap.hexprefixmatch(hexprefix)
        if len(matched) > 1:
            # TODO: Add hints about possible matches.
            raise error.RepoLookupError(
                _("%s@%s: ambiguous identifier") % (self.indexfile, hexprefix)
            )
        elif len(matched) == 1:
            return matched[0]
        else:
            return None

    def shortest(self, hexnode, minlength=1):
        def isvalid(test):
            try:
                if self._partialmatch(test) is None:
                    return False

                return True
            except error.RepoLookupError:
                return False
            except error.RevlogError:
                return False
            except error.WdirUnsupported:
                # single 'ff...' match
                return True

        shortest = hexnode
        startlength = max(6, minlength)
        length = startlength
        while True:
            test = hexnode[:length]
            if isvalid(test):
                shortest = test
                if length == minlength or length > startlength:
                    return shortest
                length -= 1
            else:
                length += 1
                if len(shortest) <= length:
                    return shortest

    def ancestors(self, revs, stoprev=0, inclusive=False):
        """Return ::revs (in revs) if inclusive is True.

        If inclusive is False, return ::parents(revs).
        If stoprev is not zero, filter the result.
        stoprev is ignored in the Rust implementation.
        """
        nodes = self.tonodes(revs)
        dag = self.dag
        if not inclusive:
            nodes = dag.parents(nodes)
        ancestornodes = dag.ancestors(nodes)
        return self.torevset(ancestornodes)

    def commonancestorsheads(self, a, b):
        """Return heads(::a & ::b)"""
        # null special case
        if nullid == a or nullid == b:
            return []
        return list(self.dag.gcaall([a, b]))

    def parents(self, node, fillnullid=True):
        """Get parent nodes of a node

        If fillnullid is set to True, ensure the result contains at least 2
        items. If there are 1 or 0 parents, fill the rest with nullid.
        """
        if fillnullid:
            # special case for null
            if node == nullid:
                return (nullid, nullid)
            parents = list(self.dag.parentnames(node))
            while len(parents) < 2:
                parents.append(nullid)
        else:
            if node == nullid:
                return []
            parents = list(self.dag.parentnames(node))
        return parents

    def parentrevs(self, rev):
        return list(map(self.rev, self.parents(self.node(rev))))

    # Revlog-related APIs used in other places.

    def revdiff(self, rev1, rev2):
        """return or calculate a delta between two revisions"""
        return mdiff.textdiff(self.revision(rev1), self.revision(rev2))

    def deltaparent(self, rev):
        # Changelog does not have deltaparent
        return nullrev

    def iscensored(self, rev):
        return False

    def candelta(self, baserev, rev):
        return True


class nodemap:
    def __init__(self, changelog):
        self.changelog = changelog

    def __getitem__(self, node):
        rev = self.get(node)
        if rev is None:
            raise error.RevlogError(_("cannot find rev for %s") % hex(node))
        else:
            return rev

    def __setitem__(self, node, rev):
        if self.get(node) != rev:
            raise error.ProgrammingError("nodemap by Rust DAG is immutable")

    def __delitem__(self, node):
        raise error.ProgrammingError("nodemap by Rust DAG is immutable")

    def __contains__(self, node):
        return node in self.changelog.idmap

    def get(self, node, default=None):
        idmap = self.changelog.idmap
        if node not in idmap:
            return default
        else:
            return idmap.node2id(node)

    def destroying(self):
        pass


# Old storage (revlog) and protocols (bundle2 - aka. addgroup()) use
# "stepparents" extras to store >2 parents. This needs to be converted
# back to real parents for newer backends.
#
# "stepparents" contains hex nodes joined by ",".
#
#                   | >2 parents  | >2 parents via |
#                   | via parents | commit extra   |
# --------------------------------------------------
# storage: dag      | yes         | no             |
# storage: revlog   | no [1]      | yes            |
# protocol: bundle2 | no          | yes            |
# protocol: edenapi | yes         | ignore         |
#
# [1]: support >2 parents in memory, but discard on flush
#
# See also D30686450.
def parse_stepparents(extras):
    """parse extra parent nodes from commit extras -> [node]"""
    if extras:
        stepparents = extras.get("stepparents")
        if stepparents:
            return [bin(h) for h in stepparents.split(",")]
    return []


def migrateto(repo, name):
    """Migrate from the current format to the destination format `name`."""
    if backendname(repo) == name:
        # Already in desired format.
        return
    if "hgsql" in repo.requirements:
        raise error.Abort(_("cannot migrate hgsql repo"))
    if "lazychangelog" in repo.storerequirements and name == "lazytext":
        # Special case: lazy -> lazytext. This is supported for testing cases.
        return migratelazytolazytext(repo)
    if "lazytextchangelog" in repo.storerequirements and name not in {
        "lazytext",
        "lazy",
    }:
        raise error.Abort(_("cannot migrate away from lazytext backend"))
    if "lazychangelog" in repo.storerequirements:
        if name != "lazy":
            raise error.Abort(_("cannot migrate away from lazy backend"))
        else:
            # No need to migrate.
            return
    if name == "revlog" or name == "rustrevlog":
        migratetorevlog(repo)
    elif name == "doublewrite":
        migratetodoublewrite(repo)
    elif name == "hybrid":
        migratetohybird(repo)
    elif name == "lazytext":
        migratetolazytext(repo)
    elif name == "lazy":
        migratetolazy(repo)
    elif name == "fullsegments":
        migratetosegments(repo)
    else:
        raise error.Abort(_("invalid changelog format: %s") % name)


def _segmentsdir(svfs):
    """Return the directory for the "segments" data.

    On Windows, this function will attempt to rename left-over directory
    (SEGMENTS_DIR_NEXT to SEGMENTS_DIR), and return SEGMENTS_DIR_NEXT if
    the rename fails. See _migratetosparsesegments for details.

    Note: the above rename could cause race conditions where another
    command might fail. But it is one time so the chance is considered
    rare.
    """
    if util.iswindows and svfs.exists(SEGMENTS_DIR_NEXT):
        # Attempt to rename SEGMENTS_DIR_NEXT to SEGMENTS_DIR.
        tmpreldir = "%s.%s" % (SEGMENTS_DIR, time.strftime("%Y%m%d_%H%M%S"))
        try:
            if svfs.exists(SEGMENTS_DIR):
                svfs.rename(SEGMENTS_DIR, tmpreldir)
            svfs.rename(SEGMENTS_DIR_NEXT, SEGMENTS_DIR)
        except Exception:
            # Cannot rename because the files are still being used.
            return svfs.join(SEGMENTS_DIR_NEXT)
    return svfs.join(SEGMENTS_DIR)


def migratetodoublewrite(repo, requirename="doublewritechangelog"):
    """Migrate to "double write" backend.

    Commit graph and IdMap use segments, commit text falls back to revlog.
    This can take about 1 minute for a large repo.
    """
    if requirename in repo.storerequirements:
        return
    svfs = repo.svfs
    revlogdir = svfs.join("")
    segmentsdir = svfs.join(SEGMENTS_DIR)
    hgcommitsdir = svfs.join(HGCOMMITS_DIR)
    with repo.lock():
        master = list(repo.nodes("present(%s)", bookmod.mainbookmark(repo)))
        with progress.spinner(repo.ui, _("migrating commit graph")):
            bindings.dag.commits.migraterevlogtosegments(
                revlogdir, segmentsdir, hgcommitsdir, master
            )
        _removechangelogrequirements(repo)
        repo.storerequirements.add(requirename)
        repo._writestorerequirements()
        repo.invalidatechangelog()


def migratetolazytext(repo):
    """Migrate to "lazytext" backend.

    The migration can only be done from hybrid or doublewrite.
    """

    if repo.nullableedenapi is None:
        # Require EdenAPI to migrate.
        repo.ui.note_err(_("cannot migrate to lazytext backend without edenapi\n"))
        return

    # Migrate revlog to segments on demand.
    if repo.changelog.algorithmbackend == "revlog":
        migratetodoublewrite(repo)

    # Migration from doublewrite or hybrid backends is a no-op.
    with repo.lock():
        _removechangelogrequirements(repo)
        repo.storerequirements.add("lazytextchangelog")
        repo._writestorerequirements()
        repo.invalidatechangelog()


def migratetolazy(repo):
    """Migrate to "lazy" backend.

    The migration can only be done from hybrid or doublewrite, or lazytext.
    The migration cannot be undone.
    """
    if "lazychangelog" in repo.storerequirements:
        # No need to migrate.
        return

    if repo.nullableedenapi is None:
        # Require EdenAPI to migrate.
        repo.ui.note_err(_("cannot migrate to lazy backend without edenapi\n"))
        return

    if util.iswindows and repo.svfs.exists(SEGMENTS_DIR_NEXT):
        repo.ui.note_err(_("cannot migrate to lazy backend with pending migration\n"))
        return

    # Migrate revlog to segments on demand.
    if repo.changelog.algorithmbackend == "revlog":
        migratetodoublewrite(repo)

    if not any(
        s in repo.storerequirements
        for s in (
            "lazytextchangelog",
            "hybridchangelog",
            "doublewritechangelog",
            "lazytext",
        )
    ) and not _isempty(repo):
        raise error.Abort(
            _(
                "lazy backend can only be migrated from hybrid or doublewrite, or lazytext"
            )
        )

    # Migration from doublewrite or hybrid backends is a no-op.
    with repo.lock():
        ml = repo.metalog()
        # Truncate metalog because older references will be invalidated.
        ml.compact(ml.path())
        # Reload metalog
        repo.invalidatemetalog()
        _migratetosparsesegments(repo)
        _removechangelogrequirements(repo)
        repo.storerequirements.add("lazychangelog")
        repo._writestorerequirements()
        repo.invalidatechangelog()


def _migratetosparsesegments(repo):
    """Migrate segments to sparse changelog segments (ex. commit hashes are lazy)

    This will also drop invisible heads.
    """
    heads = repo.heads()
    srcdir = repo.svfs.join(SEGMENTS_DIR)
    tmpreldir = "%s.%s" % (SEGMENTS_DIR, time.strftime("%Y%m%d_%H%M%S"))
    tmpdir = repo.svfs.join(tmpreldir)
    repo.invalidatechangelog()
    try:
        bindings.dag.commits.migratesparsesegments(srcdir, tmpdir, heads)
    except Exception:
        repo.svfs.rmtree(tmpreldir, ignore_errors=True)
        raise
    if util.iswindows:
        # On Windows we cannot rename segments/v1 directory easily.
        # Let's just write a special file so we can use it next time.
        repo.svfs.rename(tmpreldir, SEGMENTS_DIR_NEXT)
    else:
        repo.svfs.rename(SEGMENTS_DIR, "%s.bak" % tmpreldir)
        repo.svfs.rename(tmpreldir, SEGMENTS_DIR)


def migratetohybird(repo):
    """Migrate to "hybrid" backend.

    The migration is similar to doublewrite.
    """
    migratetodoublewrite(repo, "hybridchangelog")


def migratelazytolazytext(repo):
    """Migrate from lazy to lazytext.

    This is done by fetching all commits' ids.
    """
    if not util.istest():
        raise error.Abort(
            _("migrating from lazy to non-lazy is only supported in tests\n")
        )
    with repo.lock():
        # fetch all commits, the iterctx() interface handles batching.
        # (use revs(...).prefetch("text").iterctx() to fetch text if needed)
        ctxs = list(repo.revs("sort(_all(),rev)").iterctx())
        repo.ui.note_err(_("%d commits fetched\n") % len(ctxs))

        # respect devel.segmented-changelog-rev-compat by rewriting non-master
        # group ids to ids in the master group.
        if repo.ui.configbool("devel", "segmented-changelog-rev-compat"):
            nodes = [c.node() for c in ctxs]
        else:
            nodes = []

        # write fetched commits to disk
        repo.changelog.inner.flush(nodes)

        # update requirements
        _removechangelogrequirements(repo)
        repo.storerequirements.add("lazytextchangelog")
        repo._writestorerequirements()
        repo.invalidatechangelog()


def migratetosegments(repo):
    """Migrate to full "segmentedchangelog" backend.

    Commit graph, IdMap, commit text are all backed by segmented changelog.
    This can take 10+ minutes for a large repo.
    """
    if "segmentedchangelog" in repo.storerequirements:
        return
    svfs = repo.svfs
    revlogdir = svfs.join("")
    segmentsdir = svfs.join(SEGMENTS_DIR)
    hgcommitsdir = svfs.join(HGCOMMITS_DIR)
    with repo.lock():
        zstore = bindings.zstore.zstore(svfs.join(HGCOMMITS_DIR))
        cl = repo.changelog
        with progress.bar(
            repo.ui, _("migrating commit text"), _("commits"), len(cl)
        ) as prog:
            clnode = cl.node
            clparents = cl.parents
            clrevision = cl.revision
            contains = zstore.__contains__
            insert = zstore.insert
            textwithheader = changelogmod.textwithheader
            try:
                for rev in cl.revs():
                    node = clnode(rev)
                    if contains(node):
                        continue
                    text = clrevision(rev)
                    p1, p2 = clparents(node)
                    newnode = insert(textwithheader(text, p1, p2))
                    assert node == newnode
                    prog.value += 1
            finally:
                # In case of Ctrl+C, flush commits in memory so we can continue
                # next time.
                zstore.flush()

        master = list(repo.nodes("present(%s)", bookmod.mainbookmark(repo)))
        with progress.spinner(repo.ui, _("migrating commit graph")):
            bindings.dag.commits.migraterevlogtosegments(
                revlogdir, segmentsdir, hgcommitsdir, master
            )

        _removechangelogrequirements(repo)
        repo.storerequirements.add("segmentedchangelog")
        repo._writestorerequirements()
        repo.invalidatechangelog()


def migratetorevlog(repo):
    """Migrate to revlog backend."""
    with repo.lock():
        # Migrate from segmentedchangelog
        needmigrate = False
        if "segmentedchangelog" in repo.storerequirements:
            needmigrate = True
        if git.isgitformat(repo):
            raise error.Abort(_("cannot migrate git repo"))
        if needmigrate:
            srccl = repo.changelog
            dstcl = changelog.openrevlog(repo, repo.ui.uiconfig())
            with progress.bar(
                repo.ui, _("migrating commits"), _("commits"), len(srccl)
            ) as prog:
                hasnode = dstcl.hasnode
                getparents = srccl.dag.parentnames
                gettext = srccl.inner.getcommitrawtext
                addcommits = dstcl.inner.addcommits
                try:
                    for node in srccl.dag.all().iterrev():
                        if hasnode(node):
                            continue
                        parents = getparents(node)
                        btext = gettext(node)
                        addcommits([(node, parents, btext)])
                        prog.value += 1
                finally:
                    # In case of Ctrl+C, flush commits in memory so we can continue
                    # next time.
                    dstcl.inner.flush([])
        _removechangelogrequirements(repo)
        repo._writestorerequirements()
        repo.invalidatechangelog()


def _removechangelogrequirements(repo):
    repo.storerequirements.discard("doublewritechangelog")
    repo.storerequirements.discard("hybridchangelog")
    repo.storerequirements.discard("lazytextchangelog")
    repo.storerequirements.discard("lazychangelog")
    repo.storerequirements.discard("pythonrevlogchangelog")
    repo.storerequirements.discard("rustrevlogchangelog")
    repo.storerequirements.discard("segmentedchangelog")


def _isempty(repo):
    return len(repo.changelog) == 0


_BACKEND_REQUIREMENT_MAP = [
    # Order matters, first match wins.
    # For example, requirements with ["segmentedchangelog", "lazychangelog"]
    # would return "lazy" as backend, not "fullsegments".
    ("lazytext", "lazytextchangelog"),
    ("lazy", "lazychangelog"),
    ("hybrid", "hybridchangelog"),
    ("doublewritechangelog", "doublewrite"),
    ("fullsegments", "segmentedchangelog"),
]


def backendname(repo):
    """Obtain the changelog backend name that can be used as a migrate name"""
    for name, req in _BACKEND_REQUIREMENT_MAP:
        if req in repo.storerequirements:
            return name
    # Fallback
    return "revlog"


def _tryremove(repo, svfs, name):
    """Attempt to remote a file from svfs.
    Failures such as EPERM are ignored.
    """
    try:
        if svfs.isdir(name):
            svfs.rmtree(name)
        else:
            svfs.unlink(name)
        repo.ui.note_err(_("removed backup file %s\n") % name)
    except Exception as e:
        repo.ui.status_err("cannot remove backup file %s: %s\n" % (name, e))


def removebackupfiles(repo):
    """Attempt to remove backup files to free disk space.

    Only works for the lazy changelog backend. Has no effects for other
    changelog backends.
    """
    # Only works for lazy changelog.
    if "lazychangelog" not in repo.storerequirements:
        return

    # Bypass svfs file name encoding. (Rust logic does not respect them)
    svfs = vfsmod.vfs(repo.svfs.join(""))

    # Remove 00changelog*, hgcommits.tmp.*, segments.old.*
    for name in sorted(svfs.listdir()):
        if (
            name.startswith("00changelog.")
            or name.startswith("hgcommits.tmp.")
            or name.startswith("segments.old.")
        ):
            _tryremove(repo, svfs, name)
    # Remove segments/v1.bak*
    if svfs.isdir("segments"):
        for name in svfs.listdir("segments"):
            if name.startswith("v1.") and name.endswith(".bak"):
                _tryremove(repo, svfs, "segments/%s" % name)
