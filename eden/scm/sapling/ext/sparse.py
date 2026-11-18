# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# sparse.py - allow sparse checkouts of the working directory

"""allow sparse checkouts of the working directory

Sparse file format
------------------

Structure
.........

Shared sparse profile files comprise of 4 sections: `%include` directives
that pull in another sparse profile, and `[metadata]`, `[include]` and
`[exclude]` sections.

Any line starting with a `;` or `#` character is a comment and is ignored.

Extending existing profiles
...........................

`%include <absolute path>` directives (one per line) let you extend as
an existing profile file, adding more include and exclude rules. Although
this directive can appear anywhere in the file, it is recommended you
keep these at the top of the file.

Metadata
........

The `[metadata]` section lets you specify key-value pairs for the profile.
Anything before the first `:` or `=` is the key, everything after is the
value. Values can be extended over multiple lines by indenting additional
lines.

Only the `title`, `description` and `hidden` keys carry meaning to for
`hg sparse`, these are used in the `hg sparse list` and
`hg sparse explain` commands. Profiles with the `hidden` key (regardless
of its value) are excluded from the `hg sparse list` listing unless
the `-v` / `--verbose` switch is given.

Include and exclude rules
.........................

Each line in the `[include]` and `[exclude]` sections is treated as a
standard pattern, see :prog:`help patterns`. Exclude rules override include
rules.

Example
.......

::

  # this profile extends another profile, incorporating all its rules
  %include some/base/profile

  [metadata]
  title: This is an example sparse profile
  description: You can include as much metadata as makes sense for your
    setup, and values can extend over multiple lines.
  lorem ipsum = Keys and values are separated by a : or =
  ; hidden: the hidden key lets you mark profiles that should not
  ;  generally be discorable. The value doesn't matter, use it to motivate
  ;  why it is hidden.

  [include]
  foo/bar/baz
  bar/python_project/**/*.py

  [exclude]
  ; exclude rules override include rules, so all files with the extension
  ; .ignore are excluded from this sparse profile.
  foo/bar/baz/*.ignore

Configuration options
---------------------

The following config option defines whether sparse treats supplied
paths as relative to repo root or to the current working dir for
include and exclude options:

    [sparse]
    includereporootpaths = off

The following config option defines whether sparse treats supplied
paths as relative to repo root or to the current working dir for
enableprofile and disableprofile options:

    [sparse]
    enablereporootpaths = on

You can configure a path to find sparse profiles in; this path is
used to discover available sparse profiles. Nested directories are
reflected in the UI.

    [sparse]
    profile_directory = tools/scm/sparse

It is not set by default.

It is also possible to show hints where dirstate size is too large.

    [sparse]
    # Whether to advertise usage of the sparse profiles when the checkout size
    # is very large.
    largecheckouthint = False
    # The number of files in the checkout that constitute a "large checkout".
    largecheckoutcount = 0

The following options can be used to tune the behaviour of tree prefetching when sparse profile changes

    [sparse]
    force_full_prefetch_on_sparse_profile_change = False
"""

import collections
import functools
import hashlib
import os
import re
from typing import Any, Callable, Dict, List, Optional, Pattern, Tuple

from sapling import (
    cmdutil,
    commands,
    context,
    dirstate,
    dispatch,
    error,
    extensions,
    hg,
    hintutil,
    json,
    localrepo,
    match as matchmod,
    merge as mergemod,
    minirst,
    patch,
    pathutil,
    progress,
    registrar,
    scmutil,
    ui as uimod,
    util,
)
from sapling.i18n import _
from sapling.node import nullid, nullrev
from sapling.thirdparty import attr

cmdtable = {}
command = registrar.command(cmdtable)
testedwith = "ships-with-fb-ext"
colortable = {
    "sparse.profile.active": "brightyellow:yellow+bold",
    "sparse.profile.included": "yellow",
    "sparse.profile.inactive": "brightblack:black+bold",
    "sparse.include": "brightgreen:green+bold",
    "sparse.exclude": "brightred:red+bold",
    "sparse.profile.notfound": "brightcyan:cyan+bold",
}

cwdrealtivepatkinds = ("glob", "relpath")

profilecachefile = "sparseprofileconfigs"


def uisetup(ui) -> None:
    _setupupdates(ui)
    _setupcommit(ui)


def extsetup(ui) -> None:
    extensions.wrapfunction(dispatch, "runcommand", _tracktelemetry)
    _setupclone(ui)
    _setuplog(ui)
    _setupadd(ui)
    _setupdirstate(ui)
    _setupdiff(ui)
    _setupcat(ui)


def reposetup(ui, repo) -> None:
    # The sparse extension should never be enabled in Eden repositories;
    # Eden automatically only fetches the parts of the repository that are
    # actually required.
    if "eden" in repo.requirements:
        return

    _wraprepo(ui, repo)


def replacefilecache(cls, propname: str, replacement: Callable[..., object]) -> None:
    """Replace a filecache property with a new class. This allows changing the
    cache invalidation condition."""
    origcls = cls
    assert callable(replacement)
    while cls is not object:
        if propname in cls.__dict__:
            orig = cls.__dict__[propname]
            setattr(cls, propname, replacement(orig))
            break
        cls = cls.__bases__[0]

    if cls is object:
        raise AttributeError(_("type '%s' has no property '%s'") % (origcls, propname))


def _getsparseflavor(repo):
    return "filteredfs" if "eden" in repo.requirements else "sparse"


def _isedensparse(repo):
    return "edensparse" in repo.requirements


def _abortifnotsparse(repo) -> None:
    """Aborts if the repo is not "sparse". There are two kinds of sparse repos:
    1) non-eden-sparse (i.e legacy sparse)
    2) edensparse (i.e. FilteredHg/FilteredFS)
    """
    if "eden" in repo.requirements and "edensparse" not in repo.requirements:
        raise error.Abort(
            _(
                "You're using an Eden repo and thus don't need sparse profiles.  "
                "See https://fburl.com/new-to-eden and enjoy!"
            )
        )

    if not hasattr(repo, "sparsematch"):
        raise error.Abort(_(f"this is not a {_getsparseflavor(repo)} repository"))


def _abortifnotregularsparse(repo) -> None:
    """Aborts if the repo is eden or edensparse. Only non-eden-sparse
    (i.e legacy sparse) repos will avoid aborting.
    """
    _abortifnotsparse(repo)

    if "edensparse" in repo.requirements:
        raise error.Abort(
            _(
                "You're using an Edensparse repo and thus don't need sparse profile commands. "
                "See `@prog@ help filteredfs` for more information."
            )
        )


def _hassparse(repo):
    return hasattr(repo, "sparsematch") and (
        "eden" not in repo.requirements or "edensparse" in repo.requirements
    )


def _setupupdates(_ui) -> None:
    def _calculateupdates(
        orig,
        to_repo,
        wctx,
        mctx,
        ancestors,
        branchmerge,
        force,
        acceptremote,
        followcopies,
        from_repo=None,
    ):
        """Filter updates to only lay out files that match the sparse rules."""
        ui = to_repo.ui
        from_repo = from_repo or to_repo
        is_crossrepo = not to_repo.is_same_repo(from_repo)

        actions = orig(
            to_repo,
            wctx,
            mctx,
            ancestors,
            branchmerge,
            force,
            acceptremote,
            followcopies,
            from_repo=from_repo,
        )

        # If the working context is in memory (virtual), there's no need to
        # apply the user's sparse rules at all (and in fact doing so would
        # cause unexpected behavior in the real working copy).
        if not _hassparse(to_repo) or wctx.isinmemory():
            return actions

        files = set()
        prunedactions = {}

        # Skip calculations for edensparse repos. We can't simply move these
        # definitions to the if statement below because they must be calculated
        # prior to any temporary files being added
        oldrevs, oldsparsematch, oldprofileconfigs, newprofileconfigs = (
            None,
            None,
            None,
            None,
        )
        if not _isedensparse(to_repo):
            # Skip the p2 ctx for cross repo merge
            parents = [wctx.p1()] if is_crossrepo else wctx.parents()
            oldrevs = [pctx.rev() for pctx in parents]
            oldsparsematch = to_repo.sparsematch(*oldrevs)
            to_repo._clearpendingprofileconfig(all=True)
            oldprofileconfigs = _getcachedprofileconfigs(to_repo)
            newprofileconfigs = to_repo._creatependingprofileconfigs()

        if branchmerge:
            # If we're merging, use the wctx filter, since we're merging into
            # the wctx.
            sparsematch = to_repo.sparsematch(wctx.p1().rev())
        else:
            # If we're updating, use the target context's filter, since we're
            # moving to the target context.
            sparsematch = to_repo.sparsematch(mctx.rev())

        temporaryfiles = []
        for file, action in actions.items():
            type, args, msg = action
            files.add(file)
            if sparsematch(file):
                prunedactions[file] = action
            elif type == mergemod.ACTION_MERGE:
                temporaryfiles.append(file)
                prunedactions[file] = action
            elif branchmerge:
                if type != mergemod.ACTION_KEEP:
                    temporaryfiles.append(file)
                    prunedactions[file] = action
            elif type == mergemod.ACTION_FORGET:
                prunedactions[file] = action
            elif file in wctx:
                prunedactions[file] = (mergemod.ACTION_REMOVE, args, msg)

        if len(temporaryfiles) > 0:
            ui.status(
                _(
                    "temporarily included %d file(s) in the sparse checkout"
                    " for merging\n"
                )
                % len(temporaryfiles)
            )
            to_repo.addtemporaryincludes(temporaryfiles)

            # Add the new files to the working copy so they can be merged, etc
            actions = []
            message = "temporarily adding to sparse checkout"
            wctxmanifest = to_repo[None].manifest()
            for file in temporaryfiles:
                if file in wctxmanifest:
                    fctx = to_repo[None][file]
                    actions.append((file, (file, fctx.flags(), False), message))

            typeactions = collections.defaultdict(list)
            typeactions[mergemod.ACTION_GET] = actions
            mergemod.applyupdates(
                to_repo,
                typeactions,
                to_repo[None],
                to_repo["."],
                False,
                from_repo=from_repo,
            )

            dirstate = to_repo.dirstate
            for file, flags, msg in actions:
                dirstate.normal(file)

        # Eden handles refreshing the checkout on its own. This logic is only
        # needed for non-Eden sparse checkouts where Mercurial must refresh the
        # checkout when the sparse profile changes.
        if not _isedensparse(to_repo):
            profiles = to_repo.getactiveprofiles()
            changedprofiles = (profiles & files) or (
                oldprofileconfigs != newprofileconfigs
            )
            # If an active profile changed during the update, refresh the checkout.
            # Don't do this during a branch merge, since all incoming changes should
            # have been handled by the temporary includes above.
            if changedprofiles and not branchmerge:
                scopename = "Calculating additional actions for sparse profile update"
                with util.traced(scopename), progress.spinner(ui, "sparse config"):
                    mf = mctx.manifest()
                    fullprefetchonsparseprofilechange = ui.configbool(
                        "sparse", "force_full_prefetch_on_sparse_profile_change"
                    )
                    fullprefetchonsparseprofilechange |= not hasattr(mf, "walk")

                    with ui.configoverride(
                        {("treemanifest", "ondemandfetch"): True}, "sparseprofilechange"
                    ):
                        if fullprefetchonsparseprofilechange:
                            # We're going to need a full manifest, so if treemanifest is in
                            # use, we should prefetch. Since our tree might be incomplete
                            # (and its root could be unknown to the server if this is a
                            # local commit), we use BFS prefetching to "complete" our tree.
                            if hasattr(to_repo, "forcebfsprefetch"):
                                to_repo.forcebfsprefetch([mctx.manifestnode()])

                            iter = mf
                        else:
                            match = matchmod.xormatcher(oldsparsematch, sparsematch)
                            iter = mf.walk(match)

                        for file in iter:
                            old = oldsparsematch(file)
                            new = sparsematch(file)
                            if not old and new:
                                flags = mf.flags(file)
                                prunedactions[file] = (
                                    mergemod.ACTION_GET,
                                    (file, flags, False),
                                    "",
                                )
                            elif old and not new:
                                prunedactions[file] = (mergemod.ACTION_REMOVE, [], "")

        return prunedactions

    extensions.wrapfunction(mergemod, "calculateupdates", _calculateupdates)

    def _goto(orig, repo, node, **kwargs):
        try:
            results = orig(repo, node, **kwargs)
        except Exception:
            if _hassparse(repo) and not _isedensparse(repo):
                repo._clearpendingprofileconfig()
            raise

        # If we're updating to a location, clean up any stale temporary includes
        # (ex: this happens during hg rebase --abort).
        if _hassparse(repo):
            repo.prunetemporaryincludes()

        return results

    extensions.wrapfunction(mergemod, "goto", _goto)


def _setupcommit(ui) -> None:
    def _refreshoncommit(orig, self, node):
        """Refresh the checkout when commits touch .hgsparse"""
        orig(self, node)

        # Use unfiltered to avoid computing hidden commits
        repo = self._repo

        if _hassparse(repo):
            if _isedensparse(repo):
                # We just created a new commit that the edenfs_ffi Rust
                # repo won't know about until we flush in-memory commit
                # data to disk. Flush now to avoid unknown commit id errors
                # in EdenFS when checking edensparse contents.
                repo.changelog.inner.flushcommitdata()

            # Refresh the sparse profile so that the working copy reflects any
            # sparse (or edensparse) changes made by the new commit
            ctx = repo[node]
            profiles = getsparsepatterns(repo, ctx.rev()).allprofiles()
            if profiles & set(ctx.files()):
                origstatus = repo.status()
                origsparsematch = repo.sparsematch(
                    *list(p.rev() for p in ctx.parents() if p.rev() != nullrev)
                )
                repo._refreshsparse(repo.ui, origstatus, origsparsematch, True)

            repo.prunetemporaryincludes()

    extensions.wrapfunction(context.committablectx, "markcommitted", _refreshoncommit)


def _setuplog(ui) -> None:
    entry = commands.table.get("log", None)
    if entry is None:
        # Try command with legacy alias.
        entry = commands.table["log|history"]
    entry[1].append(
        ("", "sparse", None, "limit to changesets affecting the sparse checkout")
    )

    def _logrevs(orig, repo, opts):
        revs = orig(repo, opts)
        if opts.get("sparse"):
            _abortifnotsparse(repo)

            sparsematch = repo.sparsematch()

            def ctxmatch(rev):
                ctx = repo[rev]
                return any(f for f in ctx.files() if sparsematch(f))

            revs = revs.filter(ctxmatch)
        return revs

    extensions.wrapfunction(cmdutil, "_logrevs", _logrevs)


def _setupcat(ui) -> None:
    def _cat(orig, ui, repo, ctx, matcher, basefm, fntemplate, prefix, **opts):
        # Enforce sparse matcher check for edensparse repos. Disallows access
        # to filtered file content.
        # The edensparse requirement does not guarantee sparsematch() exists:
        # this extension's reposetup returns early for eden checkouts, and
        # edensparse's reposetup never runs when that extension is not loaded.
        # Checking the requirement alone makes every revision read raise
        # AttributeError on such a repo. Do not swap in _hassparse() here -- it
        # also matches legacy non-eden sparse repos, which this wrapper is not
        # meant to filter.
        if (
            _isedensparse(repo)
            and hasattr(repo, "sparsematch")
            and not repo.ui.configbool("sparse", "killsparsecat")
        ):
            sparsematch = repo.sparsematch()
            matcher = matchmod.intersectmatchers(matcher, sparsematch)

        return orig(ui, repo, ctx, matcher, basefm, fntemplate, prefix, **opts)

    extensions.wrapfunction(cmdutil, "cat", _cat)


def _tracktelemetry(
    runcommand: "Callable[[uimod.ui, localrepo.localrepository, Any], Optional[int]]",
    lui: "uimod.ui",
    repo: "localrepo.localrepository",
    *args: "Any",
) -> "Optional[int]":
    res = runcommand(lui, repo, *args)
    if repo is not None and repo.local():
        try:
            _tracksparseprofiles(lui, repo)
            _trackdirstatesizes(lui, repo)
        except error.Abort as ex:
            # Ignore Abort errors that occur trying to compute the telemetry data, and
            # don't let this fail the command.  For instance, reading the dirstate could
            # fail with the error "working directory state may be changed parallelly"
            lui.debug("error recording dirstate telemetry: %s\n" % (ex,))

    return res


def _tracksparseprofiles(lui: "uimod.ui", repo: "localrepo.localrepository") -> None:
    # Reading the sparse profile from the repo can potentially trigger
    # tree or file fetchings that are quite expensive. Do not read
    # them. Only read the sparse file on the filesystem.
    if hasattr(repo, "getactiveprofiles"):
        profile = repo.localvfs.tryread("sparse")
        lui.log("sparse_profiles", "", active_profiles=profile.decode())


def _trackdirstatesizes(lui: "uimod.ui", repo: "localrepo.localrepository") -> None:
    dirstate = repo.dirstate
    dirstatesize = None
    try:
        # Flat dirstate.
        dirstatesize = len(dirstate._map._map)
    except AttributeError:
        # Treestate (including eden):
        dirstatesize = len(dirstate._map._tree)
    if dirstatesize is not None:
        lui.log("dirstate_size", dirstate_size=dirstatesize)
        if (
            repo.ui.configbool("sparse", "largecheckouthint")
            and dirstatesize >= (repo.ui.configint("sparse", "largecheckoutcount") or 0)
            and (_hassparse(repo) and not _isedensparse(repo))
        ):
            hintutil.trigger("sparse-largecheckout", dirstatesize, repo)


def _clonesparsecmd(orig, ui, repo, *args, **opts):
    include_pat = opts.get("include")
    exclude_pat = opts.get("exclude")
    enableprofile_pat = opts.get("enable_profile")
    include = exclude = enableprofile = False
    if include_pat:
        pat = include_pat
        include = True
    if exclude_pat:
        pat = exclude_pat
        exclude = True
    if enableprofile_pat:
        pat = enableprofile_pat
        enableprofile = True
    if sum([include, exclude, enableprofile]) > 1:
        raise error.Abort(_("too many flags specified."))
    if include or exclude or enableprofile:

        def clone_sparse(orig, self, node, overwrite, *args, **kwargs):
            # sparse clone is a special snowflake as in that case always
            # are outside of the repo's dir hierarchy, yet we always want
            # to name our includes/excludes/enables using repo-root
            # relative paths
            overrides = {
                ("sparse", "includereporootpaths"): True,
                ("sparse", "enablereporootpaths"): True,
            }
            with self.ui.configoverride(overrides, "sparse"):
                _config(
                    self.ui,
                    self,
                    pat,
                    {},
                    include=include,
                    exclude=exclude,
                    enableprofile=enableprofile,
                )
            ret = orig(self, node, overwrite, *args, **kwargs)
            if enableprofile:
                _checknonexistingprofiles(ui, self, pat)
            return ret

        extensions.wrapfunction(hg, "updaterepo", clone_sparse)
    return orig(ui, repo, *args, **opts)


def _setupclone(ui) -> None:
    entry = commands.table["clone"]
    entry[1].append(("", "enable-profile", [], "enable a sparse profile"))
    entry[1].append(("", "include", [], "include sparse pattern"))
    entry[1].append(("", "exclude", [], "exclude sparse pattern"))
    extensions.wrapcommand(commands.table, "clone", _clonesparsecmd)


def _setupadd(ui) -> None:
    entry = commands.table["add"]
    entry[1].append(
        (
            "s",
            "sparse",
            None,
            "also include directories of added files in sparse config",
        )
    )

    def _add(orig, ui, repo, *pats, **opts):
        if opts.get("sparse"):
            dirs = set()
            for pat in pats:
                dirname, basename = util.split(pat)
                dirs.add(dirname)
            _config(ui, repo, list(dirs), opts, include=True)
        return orig(ui, repo, *pats, **opts)

    extensions.wrapcommand(commands.table, "add", _add)


def _setupdirstate(ui) -> None:
    """Modify the dirstate to prevent stat'ing excluded files,
    and to prevent modifications to files outside the checkout.
    """

    def _dirstate(orig, repo):
        dirstate = orig(repo)
        dirstate.repo = repo
        return dirstate

    extensions.wrapfunction(localrepo.localrepository.dirstate, "func", _dirstate)

    # The atrocity below is needed to wrap dirstate._ignore. It is a cached
    # property, which means normal function wrapping doesn't work.
    class ignorewrapper:
        def __init__(self, orig):
            self.orig = orig
            self.origignore = None
            self.func = None
            self.sparsematch = None

        def __get__(self, obj, type=None):
            repo = obj.repo if hasattr(obj, "repo") else None
            origignore = self.orig.__get__(obj)
            if repo is None or not _hassparse(repo):
                return origignore

            sparsematch = repo.sparsematch()
            if self.sparsematch != sparsematch or self.origignore != origignore:
                self.func = ignorematcher(origignore, negatematcher(sparsematch))
                self.sparsematch = sparsematch
                self.origignore = origignore
            return self.func

    replacefilecache(dirstate.dirstate, "_ignore", ignorewrapper)

    # dirstate.rebuild should not add non-matching files
    def _rebuild(orig, self, parent, allfiles, changedfiles=None, exact=False):
        if exact:
            # If exact=True, files outside "changedfiles" are assumed unchanged.
            # In this case, do not check files outside sparse profile. This
            # skips O(working copy) scans, and affect absorb perf.
            return orig(self, parent, allfiles, changedfiles, exact=exact)

        if (
            hasattr(self, "repo")
            and _hassparse(self.repo)
            and not _isedensparse(self.repo)
        ):
            with progress.spinner(ui, "applying sparse profile"):
                matcher = self.repo.sparsematch()
                allfiles = allfiles.matches(matcher)
                if changedfiles:
                    changedfiles = [f for f in changedfiles if matcher(f)]

                if changedfiles is not None:
                    # In _rebuild, these files will be deleted from the dirstate
                    # when they are not found to be in allfiles
                    # This is O(working copy) and is expensive.
                    dirstatefilestoremove = set(f for f in self if not matcher(f))
                    changedfiles = dirstatefilestoremove.union(changedfiles)

        return orig(self, parent, allfiles, changedfiles)

    extensions.wrapfunction(dirstate.dirstate, "rebuild", _rebuild)

    # Prevent adding files that are outside the sparse checkout
    editfuncs = ["normal", "add", "normallookup", "copy", "remove", "merge"]
    hint = _(
        "include file with `@prog@ sparse include <pattern>` or use "
        + "`@prog@ add -s <file>` to include file directory while adding"
    )
    for func in editfuncs:

        def _wrapper(orig, self, *args):
            if hasattr(self, "repo"):
                repo = self.repo
                if _hassparse(repo):
                    dirstate = repo.dirstate
                    sparsematch = repo.sparsematch()
                    for f in args:
                        if f is not None and not sparsematch(f) and f not in dirstate:
                            raise error.Abort(
                                _("cannot add '%s' - it is outside the sparse checkout")
                                % f,
                                hint=hint,
                            )
            return orig(self, *args)

        extensions.wrapfunction(dirstate.dirstate, func, _wrapper)

    # dirstate.status should exclude files outside sparse profile
    def _status(
        orig,
        self,
        match: "Callable[[str], bool]",
        ignored: bool,
        clean: bool,
        unknown: bool,
    ) -> "scmutil.status":
        st = orig(self, match, ignored, clean, unknown)
        if hasattr(self, "repo"):
            repo = self.repo
            if _hassparse(repo):
                sparsematch = repo.sparsematch()
                st = scmutil.status(
                    *([f for f in files if sparsematch(f)] for files in st)
                )
        return st

    extensions.wrapfunction(dirstate.dirstate, "status", _status)


def _setupdiff(ui) -> None:
    entry = cmdutil.findcmd("diff", commands.table)[1]
    entry[1].append(
        ("s", "sparse", None, "only show changes in files in the sparse config")
    )

    def workingfilectxdata(orig, self):
        try:
            # Try lookup working copy first.
            return orig(self)
        except IOError:
            # Then try working copy parent if the file is outside sparse.
            if hasattr(self._repo, "sparsematch"):
                sparsematch = self._repo.sparsematch()
                if not sparsematch(self._path):
                    basectx = self._changectx._parents[0]
                    return basectx[self._path].data()
            raise

    extensions.wrapfunction(context.workingfilectx, "data", workingfilectxdata)

    def workingfilectxsize(orig, self):
        try:
            # Try lookup working copy first.
            return orig(self)
        except IOError:
            # Then try working copy parent if the file is outside sparse.
            if hasattr(self._repo, "sparsematch"):
                sparsematch = self._repo.sparsematch()
                if not sparsematch(self._path):
                    basectx = self._changectx._parents[0]
                    return basectx[self._path].size()
            raise

    extensions.wrapfunction(context.workingfilectx, "size", workingfilectxsize)

    # wrap diffhunks to filter diffs if '--sparse' is set
    def diffhunks(
        orig,
        repo,
        *args,
        match=None,
        **opts,
    ):
        sparsematch = repo.sparsematch()
        if match is None:
            match = sparsematch
        else:
            match = matchmod.intersectmatchers(match, sparsematch)
        return orig(repo, *args, match=match, **opts)

    def diff(orig, ui, repo, *pats, **opts):
        issparse = False
        # Make sure --sparse option is just ignored when it's not
        # a sparse repo.
        if _hassparse(repo):
            issparse = bool(opts.get("sparse"))
        if issparse:
            extensions.wrapfunction(patch, "diffhunks", diffhunks)
        try:
            orig(ui, repo, *pats, **opts)
        finally:
            if issparse:
                extensions.unwrapfunction(patch, "diffhunks", diffhunks)

    extensions.wrapcommand(commands.table, "diff", diff)


@attr.s(frozen=True, slots=True, cmp=False)
class RawSparseConfig:
    """Represents a raw, unexpanded sparse config file"""

    # Carry the entire raw contents so we can easily delegate to the
    # Rust sparse library.
    raw = attr.ib()

    path = attr.ib()
    lines = attr.ib(convert=list)
    profiles = attr.ib(convert=tuple)
    metadata = attr.ib(default=attr.Factory(dict))

    def toincludeexclude(self):
        include = []
        exclude = []
        for kind, value in self.lines:
            if kind == "include":
                include.append(value)
            elif kind == "exclude":
                exclude.append(value)
        return include, exclude

    def version(self):
        return self.metadata.get("version", "1")


@attr.s(frozen=True, slots=True, cmp=False)
class SparseConfig:
    """Represents the full sparse config as seen by the user, including config
    rules and profile rules."""

    path = attr.ib()
    mainrules = attr.ib(convert=list)
    profiles = attr.ib(convert=tuple)
    metadata = attr.ib(default=attr.Factory(dict))
    ruleorigins = attr.ib(default=attr.Factory(list))

    def toincludeexclude(self):
        include = []
        exclude = []
        for rule in self.mainrules:
            if rule[0] == "!":
                exclude.append(rule[1:])
            else:
                include.append(rule)
        return include, exclude

    # Return whether self and other_config are effectively equivalent.
    # In particular, don't compare path or metadata.
    def equivalent(self, other_config):
        return (
            self.mainrules == other_config.mainrules
            and len(self.profiles) == len(other_config.profiles)
            and all(
                x.equivalent(y) for (x, y) in zip(self.profiles, other_config.profiles)
            )
        )

    def allprofiles(self):
        allprofiles = set()
        for profile in self.profiles:
            allprofiles.add(profile.path)
            for subprofile in profile.profiles:
                allprofiles.add(subprofile)
        return allprofiles


@attr.s(frozen=True, slots=True, cmp=False)
class SparseProfile:
    """Represents a single sparse profile, with subprofiles expanded."""

    path = attr.ib()
    rules = attr.ib(convert=list)
    profiles = attr.ib(convert=tuple)
    metadata = attr.ib(default=attr.Factory(dict))
    ruleorigins = attr.ib(default=attr.Factory(list))

    # Return whether self and other_config are effectively equivalent.
    # In particular, don't compare path or metadata.
    def equivalent(self, other):
        return (
            self.rules == other.rules
            and self.profiles == other.profiles
            and self.version() == other.version()
        )

    def version(self):
        return self.metadata.get("version", "1")

    def ruleorigin(self, idx):
        if idx < len(self.ruleorigins):
            return self.ruleorigins[idx]
        else:
            return "MISSING_RULE_ORIGIN"


# metadata parsing expression
metadata_key_value: Pattern[str] = re.compile(r"(?P<key>.*)\s*[:=]\s*(?P<value>.*)")


class SparseMixin:
    def writesparseconfig(self, include, exclude, profiles):
        raw = ""
        if _isedensparse(self):
            profiles = list(profiles)
            if len(include) != 0 or len(exclude) != 0:
                raise error.ProgrammingError(
                    "the edensparse extension does not support additional includes/excludes"
                )
            raw = "".join([f"%include {p}\n" for p in sorted(profiles)])
        else:
            raw = "%s[include]\n%s\n[exclude]\n%s\n" % (
                "".join(["%%include %s\n" % p for p in sorted(profiles)]),
                "\n".join(sorted(include)),
                "\n".join(sorted(exclude)),
            )
        filter_warning = self.ui.config("sparse", "filter-warning")
        if filter_warning is not None and raw != "":
            raw = f"{filter_warning}\n\n{raw}"
        self.localvfs.writeutf8("sparse", raw)
        self.invalidatesparsecache()

    def invalidatesparsecache(self):
        if not _isedensparse(self):
            self._sparsecache.clear()

    def _refreshsparse(self, ui, origstatus, origsparsematch, force):
        """Refreshes which files are on disk by comparing the old status and
        sparsematch with the new sparsematch.

        Will raise an exception if a file with pending changes is being excluded
        or included (unless force=True).
        """
        modified, added, removed, deleted, unknown, ignored, clean = origstatus

        # Verify there are no pending changes
        pending = set()
        pending.update(modified)
        pending.update(added)
        pending.update(removed)
        sparsematch = self.sparsematch()
        abort = False
        if len(pending) > 0:
            ui.note(_("verifying pending changes for refresh\n"))
        for file in pending:
            if not sparsematch(file):
                ui.warn(_("pending changes to '%s'\n") % file)
                abort = not force
        if abort:
            raise error.Abort(_("could not update sparseness due to pending changes"))

        return self._applysparsetoworkingcopy(
            force, origsparsematch, sparsematch, pending
        )

    def _applysparsetoworkingcopy(force, origsparsematch, sparsematch, pending):
        raise error.ProgrammingError(
            _(
                "SparseMixin users must implement their own logic for applying sparse updates to the working copy."
            )
        )

    def sparsematch(self, *revs, **kwargs):
        """Returns the sparse match function for the given revs

        If multiple revs are specified, the match function is the union
        of all the revs.

        `includetemp` is used to indicate if the temporarily included file
        should be part of the matcher.

        `config` can be used to specify a different sparse profile
        from the default .hg/sparse active profile

        """
        if not revs or revs == (None,):
            revs = [
                self.changelog.rev(node)
                for node in self.dirstate.parents()
                if node != nullid
            ]
        if not revs:
            # Need a revision to read .hg/sparse
            revs = [nullrev]

        includetemp = kwargs.get("includetemp", True)

        rawconfig = kwargs.get("config")

        cachekey = self._cachekey(revs, includetemp=includetemp)

        # The raw config could be anything, which kinda circumvents the
        # expectation that we could deterministically load a sparse matcher
        # give some revs. rawconfig is only set during some debug commands,
        # so let's just not use the cache when it is present.
        if rawconfig is None:
            result = self._sparsecache.get(cachekey, None)
            if result is not None:
                return result

        result = computesparsematcher(
            self,
            revs,
            rawconfig=rawconfig,
            nocatchall=kwargs.get("nocatchall", False),
        )

        if kwargs.get("includetemp", True):
            tempincludes = self.gettemporaryincludes()
            if tempincludes:
                result = forceincludematcher(result, tempincludes)

        if rawconfig is None:
            self._sparsecache[cachekey] = result

        return result

    def _cachekey(self, revs, includetemp=False):
        sha1 = hashlib.sha1()
        for rev in revs:
            sha1.update(self[rev].hex().encode("utf8"))
        if includetemp:
            try:
                sha1.update(self.localvfs.read("tempsparse"))
            except (OSError, IOError):
                pass
        return sha1.hexdigest()

    def gettemporaryincludes(self):
        existingtemp = set()
        if self.localvfs.exists("tempsparse"):
            raw = self.localvfs.readutf8("tempsparse")
            existingtemp.update(raw.split("\n"))
        return existingtemp

    def addtemporaryincludes(self, files):
        includes = self.gettemporaryincludes()
        for file in files:
            includes.add(file)
        self._writetemporaryincludes(includes)

    def _writetemporaryincludes(self, includes):
        raw = "\n".join(sorted(includes))
        self.localvfs.writeutf8("tempsparse", raw)
        self.invalidatesparsecache()

    def prunetemporaryincludes(self):
        if self.localvfs.exists("tempsparse"):
            if not _isedensparse(self):
                origstatus = self.status()
                modified, added, removed, deleted, a, b, c = origstatus
                if modified or added or removed or deleted:
                    # Still have pending changes. Don't bother trying to prune.
                    return

            flavortext = _getsparseflavor(self)
            sparsematch = self.sparsematch(includetemp=False)
            dirstate = self.dirstate
            actions = []
            dropped = []
            tempincludes = self.gettemporaryincludes()
            for file in tempincludes:
                if file in dirstate and not sparsematch(file):
                    message = f"dropping temporarily included {flavortext} files"
                    actions.append((file, None, message))
                    dropped.append(file)

            typeactions = collections.defaultdict(list)
            typeactions[mergemod.ACTION_REMOVE] = actions
            mergemod.applyupdates(self, typeactions, self[None], self["."], False)

            # Fix dirstate
            for file in dropped:
                dirstate.untrack(file)

            self.localvfs.unlink("tempsparse")
            self.invalidatesparsecache()
            msg = _(
                "cleaned up %d temporarily added file(s) from the "
                f"{flavortext} checkout\n"
            )
            self.ui.status(msg % len(tempincludes))


def _wraprepo(ui, repo) -> None:
    class SparseRepo(repo.__class__, SparseMixin):
        def _getlatestprofileconfigs(self):
            includes = collections.defaultdict(list)
            excludes = collections.defaultdict(list)
            for key, value in self.ui.configitems("sparseprofile"):
                # Expected format:
                #   include.someid1.path/to/sparse/profile
                #   exclude.someid2.path/to/sparse/profile
                # id is unused, but allows multiple keys to contribute to the
                # same sparse profile. This is useful when rolling out several
                # separate waves of includes/excludes simultaneously.
                split = key.split(".", 2)
                if len(split) < 3:
                    continue
                section, id, name = split
                if section == "include":
                    for include in self.ui.configlist("sparseprofile", key):
                        includes[name].append(include)
                elif section == "exclude":
                    for exclude in self.ui.configlist("sparseprofile", key):
                        excludes[name].append(exclude)

            results = {}
            keys = set(includes.keys())
            keys.update(excludes.keys())
            for key in keys:
                config = ""
                if key in includes:
                    config += "[include]\n"
                    for include in includes[key]:
                        config += include + "\n"
                if key in excludes:
                    config += "[exclude]\n"
                    for exclude in excludes[key]:
                        config += exclude + "\n"
                results[key] = config

            return results

        def _creatependingprofileconfigs(self):
            """creates a new process-local sparse profile config value

            This will be read for future sparse matchers in this process.
            """
            pendingfile = _pendingprofileconfigname()
            latest = self._getlatestprofileconfigs()
            serialized = json.dumps(latest)
            self.localvfs.writeutf8(pendingfile, serialized)
            return latest

        def _clearpendingprofileconfig(self, all=False):
            """deletes all pending sparse profile config files

            all=True causes it delete all the pending profile configs, for all
            processes. This should only be used while holding the wlock, so you don't
            accidentally delete a pending file out from under another process.
            """
            self.invalidatesparsecache()
            prefix = "%s." % profilecachefile
            pid = str(os.getpid())
            for name in self.localvfs.listdir():
                if name.startswith(prefix):
                    suffix = name[len(prefix) :]
                    if all or suffix == pid:
                        self.localvfs.unlink(name)

        def _persistprofileconfigs(self):
            """upgrades the current process-local sparse profile config value to
            be the global value
            """
            # The pending file should exist in all cases when this code path is
            # hit. But if it doesn't this will throw an exception. That's
            # probably fine though, since that indicates something went very
            # wrong.
            pendingfile = _pendingprofileconfigname()
            self.localvfs.rename(pendingfile, profilecachefile)
            self.invalidatesparsecache()

        def invalidatecaches(self):
            self.invalidatesparsecache()
            return super(SparseRepo, self).invalidatecaches()

        def getactiveprofiles(self):
            # Use unfiltered to avoid computing hidden commits
            repo = self
            revs = [
                repo.changelog.rev(node)
                for node in repo.dirstate.parents()
                if node != nullid
            ]

            activeprofiles = set()
            for rev in revs:
                profiles = getsparsepatterns(repo, rev).allprofiles()
                activeprofiles.update(profiles)

            return activeprofiles

        def _applysparsetoworkingcopy(
            self, force, origsparsematch, sparsematch, pending
        ):
            # Calculate actions
            ui.note(_("calculating actions for refresh\n"))
            with progress.spinner(ui, "populating file set"):
                dirstate = self.dirstate
                ctx = self["."]
                added = []
                lookup = []
                dropped = []
                mf = ctx.manifest()
                # Only care about files covered by the old or the new matcher.
                unionedmatcher = matchmod.unionmatcher([origsparsematch, sparsematch])
                files = set(mf.walk(unionedmatcher))

            actions = {}

            with progress.bar(ui, _("calculating"), total=len(files)) as prog:
                for file in files:
                    prog.value += 1

                    old = origsparsematch(file)
                    new = sparsematch(file)
                    # Add files that are newly included, or that don't exist in
                    # the dirstate yet.
                    if (new and not old) or (old and new and not file in dirstate):
                        fl = mf.flags(file)
                        if self.wvfs.exists(file):
                            actions[file] = (mergemod.ACTION_EXEC, (fl,), "")
                            lookup.append(file)
                        else:
                            actions[file] = (mergemod.ACTION_GET, (file, fl, False), "")
                            added.append(file)
                    # Drop files that are newly excluded, or that still exist in
                    # the dirstate.
                    elif (old and not new) or (not (old or new) and file in dirstate):
                        dropped.append(file)
                        if file not in pending:
                            actions[file] = (mergemod.ACTION_REMOVE, [], "")

            # Verify there are no pending changes in newly included files
            if len(lookup) > 0:
                ui.note(_("verifying no pending changes in newly included files\n"))
            abort = False
            for file in lookup:
                ui.warn(_("pending changes to '%s'\n") % file)
                abort = not force
            if abort:
                raise error.Abort(
                    _(
                        "cannot change sparseness due to "
                        + "pending changes (delete the files or use --force "
                        + "to bring them back dirty)"
                    )
                )

            # Check for files that were only in the dirstate.
            for file, state in dirstate.items():
                if not file in files:
                    old = origsparsematch(file)
                    new = sparsematch(file)
                    if old and not new:
                        dropped.append(file)

            # Apply changes to disk
            if len(actions) > 0:
                ui.note(_("applying changes to disk (%d actions)\n") % len(actions))
            typeactions = {
                m: []
                for m in (
                    mergemod.ACTION_ADD,
                    mergemod.ACTION_FORGET,
                    mergemod.ACTION_GET,
                    mergemod.ACTION_ADD_MODIFIED,
                    mergemod.ACTION_CHANGED_DELETED,
                    mergemod.ACTION_DELETED_CHANGED,
                    mergemod.ACTION_REMOVE,
                    mergemod.ACTION_REMOVE_GET,
                    mergemod.ACTION_DIR_RENAME_MOVE_LOCAL,
                    mergemod.ACTION_LOCAL_DIR_RENAME_GET,
                    mergemod.ACTION_MERGE,
                    mergemod.ACTION_EXEC,
                    mergemod.ACTION_KEEP,
                    mergemod.ACTION_PATH_CONFLICT,
                    mergemod.ACTION_PATH_CONFLICT_RESOLVE,
                )
            }

            with progress.bar(ui, _("applying"), total=len(actions)) as prog:
                for f, (m, args, msg) in actions.items():
                    prog.value += 1
                    if m not in typeactions:
                        typeactions[m] = []
                    typeactions[m].append((f, args, msg))
                mergemod.applyupdates(repo, typeactions, repo[None], repo["."], False)

            # Fix dirstate
            filecount = len(added) + len(dropped) + len(lookup)
            if filecount > 0:
                ui.note(_("updating dirstate\n"))
            with progress.bar(ui, _("recording"), _("files"), filecount) as prog:
                for file in added:
                    prog.value += 1
                    dirstate.normal(file)

                for file in dropped:
                    prog.value += 1
                    dirstate.untrack(file)

                for file in lookup:
                    prog.value += 1
                    # File exists on disk, and we're bringing it back in an unknown
                    # state.
                    dirstate.normallookup(file)

            return added, dropped, lookup

    if "dirstate" in repo._filecache:
        repo.dirstate.repo = repo
    repo._sparsecache = {}
    repo.__class__ = SparseRepo


def computesparsematcher(
    repo,
    revs,
    rawconfig=Optional[RawSparseConfig],
    debugversion=None,
    nocatchall: bool = False,
):
    sparsematchers = repo._rsrepo.workingcopy().sparsematchers(
        nodes=[repo[rev].node() for rev in revs],
        raw_config=(rawconfig.raw, rawconfig.path) if rawconfig else None,
        debug_version=debugversion,
        no_catch_all=nocatchall,
    )
    if not sparsematchers:
        return matchmod.always(repo.root, "")
    else:
        m = matchmod.union(
            [matchmod.sparsematcher(m, repo.root, "") for m in sparsematchers],
            repo.root,
            "",
        )
        return m


def getsparsepatterns(
    repo,
    rev,
    rawconfig: Optional[RawSparseConfig] = None,
    debugversion=None,
    nocatchall: bool = False,
) -> SparseConfig:
    """Produce the full sparse config for a revision as a SparseConfig

    This includes all patterns from included profiles, transitively.

    if config is None, use the active profile, in .hg/sparse

    """
    # Use unfiltered to avoid computing hidden commits
    if rev is None:
        raise error.Abort(_("cannot parse sparse patterns from working copy"))

    if rawconfig is None:
        if not repo.localvfs.exists("sparse"):
            # pyre-fixme[19]: Expected 0 positional arguments.
            return SparseConfig(None, [], [])

        raw = repo.localvfs.readutf8("sparse")
        rawconfig = readsparseconfig(
            repo, raw, filename=repo.localvfs.join("sparse"), depth=0
        )
    elif not isinstance(rawconfig, RawSparseConfig):
        raise error.ProgrammingError(
            "getsparsepatterns.rawconfig must be a RawSparseConfig, not: %s" % rawconfig
        )

    profileconfigs = _getcachedprofileconfigs(repo)

    includes = set()
    excludes = set()
    # This is for files such as .hgignore and .hgsparse-base, unrelated to the .hg directory.
    rules = ["glob:.hg*"]
    ruleorigins = ["sparse.py"]
    profiles = []
    onlyv1 = True
    for kind, value in rawconfig.lines:
        if kind == "profile":
            profile = readsparseprofile(repo, rev, value, profileconfigs, depth=1)
            if profile is not None:
                profiles.append(profile)
                # v1 config's put all includes before all excludes, so
                # just create a big set of include/exclude rules and
                # we'll append them later.
                version = debugversion or profile.version()
                if version == "1":
                    for i, value in enumerate(profile.rules):
                        origin = "{} -> {}".format(
                            rawconfig.path, profile.ruleorigin(i)
                        )
                        if value.startswith("!"):
                            excludes.add((value[1:], origin))
                        else:
                            includes.add((value, origin))
                elif version == "2":
                    for i, origin in enumerate(profile.ruleorigins):
                        profile.ruleorigins[i] = "{} -> {}".format(
                            rawconfig.path, origin
                        )

                    # Do nothing. A higher layer will turn profile.rules
                    # into a matcher and compose it with the other
                    # profiles.
                    onlyv1 = False
                else:
                    raise error.ProgrammingError(
                        _("unexpected sparse profile version '%s'") % version
                    )
        elif kind == "include":
            includes.add((value, rawconfig.path))
        elif kind == "exclude":
            excludes.add((value, rawconfig.path))

    if includes:
        for rule, origin in includes:
            rules.append(rule)
            ruleorigins.append(origin)

    if excludes:
        for rule, origin in excludes:
            rules.append("!" + rule)
            ruleorigins.append(origin)

    # If all rules (excluding the default '.hg*') are exclude rules, add
    # an initial "**" to provide the default include of everything.
    if not includes and onlyv1 and not nocatchall:
        rules.insert(0, "**")
        ruleorigins.append("sparse.py")

    # pyre-fixme[19]: Expected 0 positional arguments.
    return SparseConfig(
        "<aggregated from {}>".format(rawconfig.path),
        rules,
        profiles,
        rawconfig.metadata,
        ruleorigins,
    )


def readsparseconfig(
    repo, raw, filename: Optional[str] = None, warn: bool = True, depth: int = 0
) -> RawSparseConfig:
    """Takes a string sparse config and returns a SparseConfig

    This object contains the includes, excludes, and profiles from the
    raw profile.

    The filename is used to report errors and warnings, unless warn is
    set to False.

    """
    filename = filename or "<sparse profile>"
    metadata = {}
    last_key = None
    lines = []
    profiles = []

    includesection = "[include]"
    excludesection = "[exclude]"
    metadatasection = "[metadata]"
    sections = set([includesection, excludesection, metadatasection])
    current = includesection  # no sections == includes

    uiwarn = repo.ui.warn if warn else (lambda *ignored: None)

    for i, line in enumerate(raw.splitlines(), start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith(("#", ";")):
            # empty or comment line, skip
            continue

        if stripped.startswith("%include "):
            # include another profile
            stripped = stripped[9:].strip()
            if stripped:
                lines.append(("profile", stripped))
                profiles.append(stripped)
            continue

        if stripped in sections:
            current = stripped
            continue

        if current == metadatasection:
            # Metadata parsing, INI-style format
            if line.startswith((" ", "\t")):  # continuation
                if last_key is None:
                    uiwarn(
                        _(
                            "warning: sparse profile [metadata] section "
                            "indented lines that do not belong to a "
                            "multi-line entry, ignoring, in %s:%i\n"
                        )
                        % (filename, i)
                    )
                    continue
                key, value = last_key, stripped
            else:
                match = metadata_key_value.match(stripped)
                if match is None:
                    uiwarn(
                        _(
                            "warning: sparse profile [metadata] section "
                            "does not appear to have a valid option "
                            "definition, ignoring, in %s:%i\n"
                        )
                        % (filename, i)
                    )
                    last_key = None
                    continue
                key, value = (s.strip() for s in match.group("key", "value"))
                metadata[key] = []

            metadata[key].append(value)
            last_key = key
            continue

        # inclusion or exclusion line
        if stripped.startswith("/"):
            repo.ui.warn(
                _(
                    "warning: sparse profile cannot use paths starting "
                    "with /, ignoring %s, in %s:%i\n"
                )
                % (line, filename, i)
            )
            continue
        if current == includesection:
            lines.append(("include", line))
        elif current == excludesection:
            lines.append(("exclude", line))
        else:
            repo.ui.warn(
                _("unknown sparse config line: '%s' section: '%s'\n") % (line, current)
            )

    # Edensparse only supports v2 profiles
    if _isedensparse(repo):
        metadata["version"] = ["2"]

    metadata = {key: "\n".join(value).strip() for key, value in metadata.items()}
    # pyre-fixme[19]: Expected 0 positional arguments.
    rawconfig = RawSparseConfig(raw, filename, lines, profiles, metadata)
    if _isedensparse(repo):
        include, exclude = rawconfig.toincludeexclude()
        if depth == 0 and (len(include) != 0 or len(exclude) != 0):
            raise error.ProgrammingError(
                "the edensparse extension does not support additional includes/excludes"
            )
        elif depth > 0 and len(profiles) > 0:
            raise error.Abort(
                _("'%s' is not a valid filter: filters cannot %%include other profiles")
                % filename,
                hint=_("if enabled, run '@prog@ filteredfs disable %s'") % filename,
            )

    return rawconfig


def readsparseprofile(
    repo, rev, name: Optional[str], profileconfigs, depth: int
) -> Optional[SparseProfile]:
    ctx = repo[rev]
    try:
        raw = getrawprofile(repo, name, ctx.hex())
    except error.ManifestLookupError:
        msg = "warning: sparse profile '%s' not found in rev %s - ignoring it\n" % (
            name,
            ctx,
        )
        # experimental config: sparse.missingwarning
        if repo.ui.configbool("sparse", "missingwarning"):
            repo.ui.warn(msg)
        else:
            repo.ui.debug(msg)
        return None

    rawconfig = readsparseconfig(repo, raw, filename=name, depth=depth)

    rules = []
    ruleorigins = []
    profiles = set()
    for kind, value in rawconfig.lines:
        if kind == "profile":
            if _isedensparse(repo) and depth > 1:
                raise error.Abort(
                    "the edensparse extension does not support nested filter "
                    "profiles (i.e. `%include` rules)"
                )
            profiles.add(value)
            profile = readsparseprofile(repo, rev, value, profileconfigs, depth + 1)
            if profile is not None:
                for i, rule in enumerate(profile.rules):
                    rules.append(rule)
                    ruleorigins.append("{} -> {}".format(name, profile.ruleorigin(i)))
                for subprofile in profile.profiles:
                    profiles.add(subprofile)
        elif kind == "include":
            rules.append(value)
            ruleorigins.append(name)
        elif kind == "exclude":
            rules.append("!" + value)
            ruleorigins.append(name)

    if profileconfigs:
        raw = profileconfigs.get(name)
        if raw:
            rawprofileconfig = readsparseconfig(
                repo,
                raw,
                # pyre-fixme[58]: `+` is not supported for operand types
                #  `Optional[str]` and `str`.
                filename=name + "-hgrc.dynamic",
            )
            for kind, value in rawprofileconfig.lines:
                if kind == "include":
                    rules.append(value)
                    ruleorigins.append(rawprofileconfig.path)
                elif kind == "exclude":
                    rules.append("!" + value)
                    ruleorigins.append(rawprofileconfig.path)

    # pyre-fixme[19]: Expected 0 positional arguments.
    return SparseProfile(name, rules, profiles, rawconfig.metadata, ruleorigins)


def getrawprofile(repo, profile, changeid):
    return repo.filectx(profile, changeid=changeid).data().decode()


def _getcachedprofileconfigs(repo):
    """gets the currently cached sparse profile config value

    This may be a process-local value, if this process is in the middle
    of a checkout.
    """
    # First check for a process-local profilecache. This let's an
    # ongoing checkout see the new sparse profile before we persist it
    # for other processes to see.
    pendingfile = _pendingprofileconfigname()
    for name in [pendingfile, profilecachefile]:
        if repo.localvfs.exists(name):
            serialized = repo.localvfs.readutf8(name)
            try:
                return json.loads(serialized)
            except Exception:
                continue
    return {}


def _pendingprofileconfigname() -> str:
    return "%s.%s" % (profilecachefile, os.getpid())


# A profile is either active, inactive or included; the latter is a profile
# included (transitively) by an active profile.
PROFILE_INACTIVE, PROFILE_ACTIVE, PROFILE_INCLUDED = _profile_flags = range(3)


@attr.s(slots=True, frozen=True)
class ProfileInfo(collections.abc.Mapping):
    path = attr.ib()
    active = attr.ib()
    _metadata = attr.ib(default=attr.Factory(dict))

    @active.validator
    def checkactive(self, attribute, value):
        if not any(value is flag for flag in _profile_flags):
            raise ValueError("Invalid active flag value")

    # Mapping methods for metadata access
    def __getitem__(self, key):
        return self._metadata[key]

    def __iter__(self):
        return iter(self._metadata)

    def __len__(self):
        return len(self._metadata)


def _discover(ui, repo, rev: Optional[str] = None):
    """Generate a list of available profiles with metadata

    Returns a generator yielding ProfileInfo objects, paths are relative to the
    repository root, the sequence is sorted by path.

    If no sparse.profile_directory path is configured, will only
    yield active and included profiles.

    README(.*) files are filtered out.

    If rev is given, show profiles available at that revision. The working copy
    sparse configuration is ignored and no active profile information is
    made available (all profiles are marked as 'inactive').

    """
    if not rev:
        included = repo.getactiveprofiles()
        sparse = repo.localvfs.readutf8("sparse")
        active = readsparseconfig(repo, sparse).profiles
        active = frozenset(active)
        rev = "."
    else:
        included = active = frozenset()

    profile_directory = ui.config("sparse", "profile_directory")
    available = set()
    ctx = scmutil.revsingle(repo, rev)
    if profile_directory is not None:
        if os.path.isabs(profile_directory) or profile_directory.startswith("../"):
            raise error.Abort(
                _("sparse.profile_directory must be relative to the repository root")
            )
        if not profile_directory.endswith("/"):
            profile_directory += "/"

        mf = ctx.manifest()

        matcher = matchmod.match(
            repo.root,
            repo.getcwd(),
            patterns=["path:" + profile_directory],
            exclude=[
                "relglob:README.*",
                "relglob:README",
                "relglob:.*",
                "relglob:*.py",
            ],
        )
        available.update(mf.matches(matcher))

    # sort profiles and read profile metadata as we iterate
    for p in sorted(available | included):
        try:
            raw = getrawprofile(repo, p, ctx.hex())
        except error.ManifestLookupError:
            # ignore a missing profile; this should only happen for 'included'
            # profiles, however. repo.getactiveprofiles() above will already
            # have printed a warning about such profiles.
            if p not in included:
                raise
            continue
        md = readsparseconfig(repo, raw, filename=p).metadata
        # pyre-fixme[19]: Expected 0 positional arguments.
        yield ProfileInfo(
            p,
            (
                PROFILE_ACTIVE
                if p in active
                else PROFILE_INCLUDED
                if p in included
                else PROFILE_INACTIVE
            ),
            md,
        )


def _profilesizeinfo(ui, repo, *config, **kwargs):
    """Get size stats for a given set of profiles

    Returns a dictionary of config -> (count, bytes) tuples. The
    special key `None` represents the total manifest count and
    bytecount. bytes is the total size of the files.

    Note: for performance reasons we don't calculate the total repository size
    and the value for the `None` key is always set to (count, None) to reflect
    this.

    """
    collectsize = kwargs.get("collectsize", False)

    results = {}
    matchers = {}

    rev = kwargs.get("rev", ".")
    ctx = scmutil.revsingle(repo, rev)

    templ = "sparseprofilestats:%s:{}" % util.split(repo.root)[-1]

    def _genkey(path, *parts):
        # paths need to be ascii-safe with
        path = path.replace("/", "__")
        return templ.format(":".join((path,) + parts))

    results[None] = [0, None]
    # gather complete working copy data
    matchers[None] = matchmod.always(repo.root, repo.root)

    for c in config:
        matcher = repo.sparsematch(ctx.hex(), includetemp=False, config=c)
        results[c] = [0, 0]
        matchers[c] = matcher

    if matchers:
        mf = ctx.manifest()
        if results[None][0]:
            # use cached working copy size
            totalfiles = results[None][0]
        else:
            with progress.spinner(ui, "calculating total manifest size"):
                try:
                    totalfiles = len(mf)
                except TypeError:
                    # treemanifest does not implement __len__ :-(
                    totalfiles = sum(1 for __ in mf)

        if collectsize and len(matchers) - (None in matchers):
            # we may need to prefetch file data, to calculate the size of each
            # profile
            try:
                remotefilelog = extensions.find("remotefilelog")
            except KeyError:
                pass
            else:
                if remotefilelog.shallowrepo.requirement in repo.requirements:
                    profilematchers = unionmatcher([matchers[k] for k in matchers if k])
                    repo.prefetch(repo.revs(ctx.hex()), matcher=profilematchers)

        with progress.bar(ui, _("calculating"), total=totalfiles) as prog:
            # only matchers for which there was no cache are processed
            for file in ctx.walk(unionmatcher(list(matchers.values()))):
                prog.value += 1
                for c, matcher in matchers.items():
                    if matcher(file):
                        results[c][0] += 1
                        if collectsize and c is not None:
                            results[c][1] += ctx.filectx(file).size()

    results = {k: tuple(v) for k, v in results.items()}

    return results


# hints
hint = registrar.hint()


@hint("sparse-largecheckout")
def hintlargecheckout(dirstatesize, repo) -> str:
    return (
        _(
            "Your repository checkout has %s files which makes Many mercurial "
            "commands slower. Learn how to make it smaller at "
            "https://fburl.com/hgsparse"
        )
        % dirstatesize
    )


@hint("sparse-explain-verbose")
def hintexplainverbose(*profiles) -> str:
    return _(
        "use '@prog@ sparse explain --verbose %s' to include the total file "
        "size for a give profile"
    ) % " ".join(profiles)


@hint("sparse-list-verbose")
def hintlistverbose(profiles, filters, load_matcher) -> Optional[str]:
    # move the hidden flag from the without to the with pile and count
    # the matches
    filters["with"].add("hidden")
    filters["without"].remove("hidden")
    pred = _build_profile_filter(filters, load_matcher)
    hidden_count = sum(1 for p in filter(pred, profiles))
    if hidden_count:
        return (
            _("%d hidden profiles not shown; add '--verbose' to include these")
            % hidden_count
        )


_deprecate = lambda o, l=_("(DEPRECATED)"): (
    (o[:3] + (" ".join([o[4], l]),) + o[4:]) if l not in o[4] else l
)


@command(
    "sparse",
    [
        (
            "f",
            "force",
            False,
            _("allow changing rules even with pending changes(DEPRECATED)"),
        ),
        (
            "I",
            "include",
            False,
            _("include files in the sparse checkout (DEPRECATED)"),
        ),
        (
            "X",
            "exclude",
            False,
            _("exclude files in the sparse checkout (DEPRECATED)"),
        ),
        ("d", "delete", False, _("delete an include/exclude rule (DEPRECATED)")),
        (
            "",
            "enable-profile",
            False,
            _("enables the specified profile (DEPRECATED)"),
        ),
        (
            "",
            "disable-profile",
            False,
            _("disables the specified profile (DEPRECATED)"),
        ),
        ("", "import-rules", False, _("imports rules from a file (DEPRECATED)")),
        (
            "",
            "clear-rules",
            False,
            _("clears local include/exclude rules (DEPRECATED)"),
        ),
        (
            "",
            "refresh",
            False,
            _("updates the working after sparseness changes (DEPRECATED)"),
        ),
        ("", "reset", False, _("makes the repo full again (DEPRECATED)")),
        (
            "",
            "cwd-list",
            False,
            _("list the full contents of the current directory (DEPRECATED)"),
        ),
    ]
    + [_deprecate(o) for o in commands.templateopts],
    _("SUBCOMMAND ..."),
)
def sparse(ui, repo, *pats, **opts) -> None:
    """make the current checkout sparse, or edit the existing checkout

    The sparse command is used to make the current checkout sparse.
    This means files that don't meet the sparse condition will not be
    written to disk, or show up in any working copy operations. It does
    not affect files in history in any way.

    All the work is done in subcommands such as `hg sparse enableprofile`;
    passing no subcommand prints the currently applied sparse rules.

    The `include` and `exclude` subcommands are used to add and remove files
    from the sparse checkout, while delete removes an existing include/exclude
    rule.

    Sparse profiles can also be shared with other users of the repository by
    committing a file with include and exclude rules in a separate file. Use the
    `enableprofile` and `disableprofile` subcommands to enable or disable
    such profiles. Changes to shared profiles are not applied until they have
    been committed.

    See :prog:`help -e sparse` and :prog:`help sparse [subcommand]` to get
    additional information.
    """
    _abortifnotregularsparse(repo)

    include = opts.get("include")
    exclude = opts.get("exclude")
    force = opts.get("force")
    enableprofile = opts.get("enable_profile")
    disableprofile = opts.get("disable_profile")
    importrules = opts.get("import_rules")
    clearrules = opts.get("clear_rules")
    delete = opts.get("delete")
    refresh = opts.get("refresh")
    reset = opts.get("reset")
    cwdlist = opts.get("cwd_list")
    count = sum(
        [
            include,
            exclude,
            enableprofile,
            disableprofile,
            delete,
            importrules,
            refresh,
            clearrules,
            reset,
            cwdlist,
        ]
    )
    if count > 1:
        raise error.Abort(_("too many flags specified"))

    if count == 0:
        if repo.localvfs.exists("sparse"):
            ui.status(repo.localvfs.readutf8("sparse") + "\n")
            temporaryincludes = repo.gettemporaryincludes()
            if temporaryincludes:
                ui.status(_("Temporarily Included Files (for merge/rebase):\n"))
                msg = "\n".join(temporaryincludes) + "\n"
                ui.status(msg)
        else:
            ui.status(_("repo is not sparse\n"))
        return

    if include or exclude or delete or reset or enableprofile or disableprofile:
        _config(
            ui,
            repo,
            pats,
            opts,
            include=include,
            exclude=exclude,
            reset=reset,
            delete=delete,
            enableprofile=enableprofile,
            disableprofile=disableprofile,
            force=force,
        )
        if enableprofile:
            _checknonexistingprofiles(ui, repo, pats)

    if importrules:
        _import(ui, repo, pats, opts, force=force)

    if clearrules:
        # Put the check back in to warn people about full checkouts
        _clear(ui, repo, pats, force=force)

    if refresh:
        with repo.wlock():
            c = repo._refreshsparse(ui, repo.status(), repo.sparsematch(), force)
            fcounts = list(map(len, c))
            _verbose_output(ui, opts, 0, 0, 0, *fcounts)

    if cwdlist:
        _cwdlist(repo)


subcmd = sparse.subcommand(
    categories=[
        (
            "Show information about sparse profiles",
            ["show", "list", "explain", "files"],
        ),
        ("Change which profiles are active", ["switch", "enable", "disable"]),
        (
            "Manage additional files to include or exclude",
            ["include", "uninclude", "exclude", "unexclude", "clear"],
        ),
        ("Refresh the checkout and apply sparse profile changes", ["refresh"]),
    ]
)


def _showsubcmdlogic(ui, repo, opts) -> None:
    _abortifnotsparse(repo)
    flavortext = _getsparseflavor(repo)
    if not repo.localvfs.exists("sparse"):
        if not ui.plain():
            ui.status(_(f"No {flavortext} profile enabled\n"))
        return
    raw = repo.localvfs.readutf8("sparse")
    rawconfig = readsparseconfig(repo, raw)

    profiles = rawconfig.profiles
    include, exclude = rawconfig.toincludeexclude()

    LOOKUP_SUCCESS, LOOKUP_NOT_FOUND = range(0, 2)

    def getprofileinfo(profile, depth):
        """Returns a list of (depth, profilename, title) for this profile
        and all its children."""
        try:
            raw = getrawprofile(repo, profile, ".")
        except KeyError:
            return [(depth, profile, LOOKUP_NOT_FOUND, "")]
        sc = readsparseconfig(repo, raw, filename=profile, depth=depth)

        profileinfo = [(depth, profile, LOOKUP_SUCCESS, sc.metadata.get("title"))]
        for profile in sorted(sc.profiles):
            profileinfo.extend(getprofileinfo(profile, depth + 1))
        return profileinfo

    ui.pager(f"{flavortext} show")
    with ui.formatter(f"{flavortext}", opts) as fm:
        if profiles:
            profileinfo = []
            fm.plain(_("Enabled Profiles:\n\n"))
            startingdepth = 1 if _isedensparse(repo) else 0
            profileinfo = sum(
                (getprofileinfo(p, startingdepth) for p in sorted(profiles)), []
            )
            maxwidth = max(len(name) for depth, name, lookup, title in profileinfo)
            maxdepth = max(depth for depth, name, lookup, title in profileinfo)

            for depth, name, lookup, title in profileinfo:
                if lookup == LOOKUP_SUCCESS:
                    if depth > 0:
                        label = f"{flavortext}.profile.included"
                        status = "~"
                    else:
                        label = f"{flavortext}.profile.active"
                        status = "*"
                else:
                    label = f"{flavortext}.profile.notfound"
                    status = "!"

                fm.startitem()
                fm.data(type="profile", depth=depth, status=status)
                fm.plain("  " * depth + "  " + status + " ", label=label)
                fm.write(
                    "name",
                    "%%-%ds" % (maxwidth + (maxdepth - depth) * 2),
                    name,
                    label=label,
                )
                if title:
                    fm.write("title", "  %s", title, label=label + ".title")
                fm.plain("\n")
            if include or exclude:
                fm.plain("\n")

        if include:
            fm.plain(_("Additional Included Paths:\n\n"))
            for fname in sorted(include):
                fm.startitem()
                fm.data(type="include")
                fm.write("name", "  %s\n", fname, label="sparse.include")
            if exclude:
                fm.plain("\n")

        if exclude:
            fm.plain(_("Additional Excluded Paths:\n\n"))
            for fname in sorted(exclude):
                fm.startitem()
                fm.data(type="exclude")
                fm.write("name", "  %s\n", fname, label="sparse.exclude")


@subcmd("show", commands.templateopts)
def show(ui, repo, **opts) -> None:
    """show the currently enabled sparse profile"""
    _abortifnotregularsparse(repo)
    _showsubcmdlogic(ui, repo, opts)


@command(
    "debugsparseprofilev2",
    [],
    _(""),
)
def debugsparseprofilev2(ui, repo, profile, **opts) -> None:
    """compares v1 and v2 computations of the sparse profile, printing the
    number of files matched by each, and the files that are different between
    the two.
    """
    rev = repo["."].rev()
    mf = repo["."].manifest()
    raw = (
        """
%%include %s
"""
        % profile
    )
    rawconfig = readsparseconfig(repo, raw, "<debug temp sparse config>")

    matcherv1 = computesparsematcher(repo, [rev], rawconfig=rawconfig, debugversion="1")
    files1 = set(mf.walk(matcherv1))
    print("V1 includes %s files" % len(files1))

    matcherv2 = computesparsematcher(repo, [rev], rawconfig=rawconfig, debugversion="2")
    files2 = set(mf.walk(matcherv2))
    print("V2 includes %s files" % len(files2))

    if files1 != files2:
        for file in files1 - files2:
            print("- %s" % file)
        for file in files2 - files1:
            print("+ %s" % file)


@command(
    "debugsparsematch",
    [
        ("s", "sparse-profile", [], "sparse profile to include"),
        ("x", "exclude-sparse-profile", [], "sparse profile to exclude"),
        ("0", "print0", None, _("end filenames with NUL")),
    ],
    _("-s SPARSE_PROFILE [OPTION]... FILE..."),
)
def debugsparsematch(ui, repo, *args, **opts) -> None:
    """Filter paths using the given sparse profile

    Print paths that match the given sparse profile.
    Paths are relative to repo root, and can use `listfile:` prefix.

    Unlike 'sparse files', paths to test do not have to be present in the
    working copy.
    """
    # Make it work in an edenfs checkout.
    if "eden" in repo.requirements:
        _wraprepo(ui, repo)
    profiles = opts.get("sparse_profile")
    if not profiles:
        raise error.Abort(_("--sparse-profile is required"))
    ctx = repo[None]  # use changes in cwd

    m = scmutil.match(ctx, pats=args, opts=opts, default="path")
    files = m.files()
    ui.status_err(_("considering %d file(s)\n") % len(files))

    def getmatcher(profile):
        raw = "%%include %s" % profile
        return repo.sparsematch(
            config=readsparseconfig(repo, raw=raw, filename=profile)
        )

    def getunionmatcher(profiles):
        matchers = [getmatcher(p) for p in profiles]
        if not matchers:
            return None
        else:
            return matchmod.union(matchers, "", "")

    includematcher = getunionmatcher(opts.get("sparse_profile"))
    excludematcher = getunionmatcher(opts.get("exclude_sparse_profile"))
    if excludematcher is None:
        matcher = includematcher
    else:
        matcher = matchmod.intersectmatchers(
            includematcher, negatematcher(excludematcher)
        )

    use0separator = opts.get("print0")
    for path in files:
        if matcher(path):
            if use0separator:
                ui.write(_("%s\0") % path)
            else:
                ui.write(_("%s\n") % path)


@command(
    "debugsparseexplainmatch",
    [
        ("s", "sparse-profile", "", "sparse profile to include"),
    ],
    _("-s SPARSE_PROFILE FILE..."),
)
def debugsparseexplainmatch(ui, repo, *args, **opts) -> None:
    # Make it work in an edenfs checkout.
    if "eden" in repo.requirements:
        _wraprepo(ui, repo)

    ctx = repo["."]

    config = None
    profile = opts.get("sparse_profile")
    if profile:
        if not repo.wvfs.isfile(profile):
            raise error.Abort(_("no such profile %s") % (profile))

        raw = "%%include %s" % profile
        config = readsparseconfig(repo, raw=raw, filename="<cli>")
    elif not repo.localvfs.exists("sparse"):
        # If there is no implicit sparse config (e.g. non-sparse or
        # EdenFS working copies), the user must specify a sparse
        # profile.
        raise error.Abort(_("--sparse-profile is required"))

    m = scmutil.match(ctx, pats=args, opts=opts, default="path")
    files = m.files()

    matcher = repo.sparsematch(
        config=config,
    )

    for f in files:
        explanation = matcher.explain(f)
        if not explanation:
            ui.write(_("{}: excluded by default\n".format(f)))
        else:
            ui.write(_("%s:\n") % explanation)


def _contains_files(load_matcher, profile, files) -> bool:
    matcher = load_matcher(profile)
    return all(matcher(f) for f in files)


def _build_profile_filter(filters, load_matcher):
    """Create a callable function to filter a profile, returning a boolean"""
    predicates = {
        # we need *all* fields in a with filter to be present, so with
        # should be a subset
        "with": lambda fields: fields.issubset,
        # set.isdisjoint returns true when the iterable (dictionary keys)
        # doesn't have any names in common
        "without": lambda fields: fields.isdisjoint,
        "filter": lambda field_values: lambda md: (
            # all fields to test and all values for those fields are present
            all(
                f in md and all(v in md[f].lower() for v in vs)
                for f, vs in field_values.items()
            )
        ),
        "contains_file": lambda files: lambda md: _contains_files(
            load_matcher, md["path"], files
        ),
    }
    tests = [predicates[k](v) for k, v in sorted(filters.items()) if v]
    # pass in a dictionary with all metadata and the path as an extra key
    return lambda p: all(t(dict(p, path=p.path)) for t in tests)  # dict-from-generator


@subcmd(
    "list",
    [
        (
            "r",
            "rev",
            "",
            _("explain the profile(s) against the specified revision"),
            _("REV"),
        ),
        (
            "",
            "with-field",
            [],
            _("Only show profiles that have defined the named metadata field"),
            _("FIELD"),
        ),
        (
            "",
            "without-field",
            [],
            _("Only show profiles that do have not defined the named metadata field"),
            _("FIELD"),
        ),
        (
            "",
            "filter",
            [],
            _(
                "Only show profiles that contain the given value as a substring in a "
                "specific metadata field."
            ),
            _("FIELD:VALUE"),
        ),
        (
            "",
            "contains-file",
            [],
            _("Only show profiles that would include the named file if enabled."),
            _("FILE"),
        ),
    ]
    + commands.templateopts,
    "[OPTION]...",
)
def _listprofiles(ui, repo, *pats, **opts) -> None:
    """list available sparse profiles

    Show all available sparse profiles, with the active profiles marked.

    You can filter profiles with `--with-field [FIELD]`, `--without-field
    [FIELD]`, `--filter [FIELD:VALUE]` and `--contains-file [FILENAME]`; you can
    specify these options more than once to set multiple criteria, which all
    must match for a profile to be listed. The field `path` is always available,
    and is the path of the profile file in the repository.

    `--filter` takes a fieldname and value to look for, separated by a colon.
    The field must be present in the metadata, and the value present in the
    value for that field, to match; testing is done case-insensitively.
    Multiple filters for the same fieldname are accepted and must all match;
    e.g. --filter path:foo --filter path:bar only matches profile paths with the
    substrings foo and bar both present.

    `--contains-file` takes a file path relative to the current directory. No
    check is made if the file actually exists; any profile that would include
    the file if it did exist will match.

    By default, `--without-field hidden` is implied unless you use the --verbose
    switch to include hidden profiles.

    If `--rev` is given, show profiles available at that revision. The working copy
    sparse configuration is ignored and no active profile information is
    made available (all profiles are marked as 'inactive').

    """
    _abortifnotregularsparse(repo)

    rev = scmutil.revsingle(repo, opts.get("rev")).hex()
    tocanon = functools.partial(pathutil.canonpath, repo.root, repo.getcwd())
    filters = {
        "with": set(opts.get("with_field", ())),
        "without": set(opts.get("without_field", ())),
        "filter": {},  # dictionary of fieldnames to sets of values
        "contains_file": {tocanon(f) for f in opts.get("contains_file", ())},
    }
    for fieldvalue in opts.get("filter", ()):
        fieldname, __, value = fieldvalue.partition(":")
        if not value:
            raise error.Abort(_("Missing value for filter on %s") % fieldname)
        # pyre-fixme[16]: Item `Set` of `Union[Dict[typing.Any, typing.Any],
        #  Set[typing.Any]]` has no attribute `setdefault`.
        filters["filter"].setdefault(fieldname, set()).add(value.lower())

    # It's an error to put a field both in the 'with' and 'without' buckets
    # pyre-fixme[58]: `&` is not supported for operand types `Union[Dict[typing.Any,
    #  typing.Any], typing.Set[typing.Any]]` and `Union[Dict[typing.Any, typing.Any],
    #  typing.Set[typing.Any]]`.
    if filters["with"] & filters["without"]:
        raise error.Abort(
            _(
                "You can't specify fields in both --with-field and "
                "--without-field, please use only one or the other, for "
                "%s"
            )
            # pyre-fixme[58]: `&` is not supported for operand types
            #  `Union[Dict[typing.Any, typing.Any], typing.Set[typing.Any]]` and
            #  `Union[Dict[typing.Any, typing.Any], typing.Set[typing.Any]]`.
            % ",".join(filters["with"] & filters["without"])
        )

    if not (ui.verbose or "hidden" in filters["with"]):
        # without the -v switch, hide profiles that have 'hidden' set. Unless,
        # of course, we specifically are filtering on hidden profiles!
        # pyre-fixme[16]: Item `Dict` of `Union[Dict[typing.Any, typing.Any],
        #  Set[typing.Any]]` has no attribute `add`.
        filters["without"].add("hidden")

    chars = {PROFILE_INACTIVE: "", PROFILE_INCLUDED: "~", PROFILE_ACTIVE: "*"}
    labels = {
        PROFILE_INACTIVE: "inactive",
        PROFILE_INCLUDED: "included",
        PROFILE_ACTIVE: "active",
    }
    ui.pager("sparse list")
    with ui.formatter("sparse", opts) as fm:
        fm.plain("Available Profiles:\n\n")

        load_matcher = lambda p: repo.sparsematch(
            rev,
            config=readsparseconfig(
                repo, getrawprofile(repo, p, rev), filename=p, warn=False
            ),
            nocatchall=True,
        )

        predicate = _build_profile_filter(filters, load_matcher)
        profiles = list(_discover(ui, repo, rev=opts.get("rev")))
        filtered = list(filter(predicate, profiles))
        max_width = 0
        if not filtered:
            if fm.isplain():
                ui.write_err(_("No profiles matched the filter criteria\n"))
        else:
            max_width = max(len(p.path) for p in filtered)

        for info in filtered:
            fm.startitem()
            label = "sparse.profile." + labels[info.active]
            fm.plain(" %-1s " % chars[info.active], label=label)
            fm.data(active=labels[info.active], metadata=dict(info))
            fm.write("path", "%-{}s".format(max_width), info.path, label=label)
            if "title" in info:
                fm.plain("  %s" % info.get("title", ""), label=label + ".title")
            fm.plain("\n")

    if not (ui.verbose or "hidden" in filters["with"]):
        hintutil.trigger("sparse-list-verbose", profiles, filters, load_matcher)


@subcmd(
    "explain",
    [
        (
            "r",
            "rev",
            "",
            _("explain the profile(s) against the specified revision"),
            _("REV"),
        )
    ]
    + commands.templateopts,
    _("[OPTION]... [PROFILE]..."),
)
def _explainprofile(ui, repo, *profiles, **opts) -> int:
    """show information about a sparse profile

    If --verbose is given, calculates the file size impact of a profile (slow).
    """
    # Make it work in an edenfs checkout.
    if "eden" in repo.requirements:
        _wraprepo(ui, repo)

    if ui.plain() and not opts.get("template"):
        hint = _("invoke with -T/--template to control output format")
        raise error.Abort(_("must specify a template in plain mode"), hint=hint)

    if not profiles:
        raise error.Abort(_("no profiles specified"))

    rev = scmutil.revrange(repo, [opts.get("rev") or "."]).last()
    if rev is None:
        raise error.Abort(_("empty revision set"))

    configs = []
    for i, p in enumerate(profiles):
        try:
            raw = getrawprofile(repo, p, rev)
        except KeyError:
            ui.warn(_("The profile %s was not found\n") % p)
            exitcode = 255
            continue
        rawconfig = readsparseconfig(repo, raw, p)
        configs.append(rawconfig)

    stats = _profilesizeinfo(ui, repo, *configs, rev=rev, collectsize=ui.verbose)
    filecount, totalsize = stats[None]

    exitcode = 0

    def sortedsets(d):
        return {
            k: sorted(v) if isinstance(v, collections.abc.Set) else v
            for k, v in d.items()
        }

    ui.pager("sparse explain")
    with ui.formatter("sparse", opts) as fm:
        for i, profile in enumerate(configs):
            if i:
                fm.plain("\n")
            fm.startitem()

            fm.write("path", "%s\n\n", profile.path)

            pfilecount, ptotalsize = stats.get(profile, (-1, -1))
            pfileperc = 0.0
            if pfilecount > -1 and filecount > 0:
                pfileperc = (pfilecount / filecount) * 100
            profilestats = {"filecount": pfilecount, "filecountpercentage": pfileperc}
            if ptotalsize:
                profilestats["totalsize"] = ptotalsize
            fm.data(
                stats=profilestats,
                **sortedsets(attr.asdict(profile, retain_collection_types=True)),
            )

            if fm.isplain():
                md = profile.metadata
                title = md.get("title", _("(untitled)"))
                lines = [minirst.section(title)]
                description = md.get("description")
                if description:
                    lines.append("%s\n\n" % description)

                if pfileperc or ptotalsize:
                    lines.append(
                        minirst.subsection(_("Size impact compared to a full checkout"))
                    )

                    if pfileperc:
                        lines.append(
                            ":file count: {:d} ({:.2f}%)\n".format(
                                pfilecount, pfileperc
                            )
                        )
                    if ptotalsize:
                        lines.append(
                            ":total size: {:s}\n".format(util.bytecount(ptotalsize))
                        )
                    lines.append("\n")

                other = set(md.keys()) - {"title", "description"}
                if other:
                    lines += (
                        minirst.subsection(_("Additional metadata")),
                        "".join(
                            [
                                ":%s: %s\n" % (key, "\n  ".join(md[key].splitlines()))
                                for key in sorted(other)
                            ]
                        ),
                        "\n",
                    )

                sections = (
                    ("profiles", _("Profiles included")),
                    ("includes", _("Inclusion rules")),
                    ("excludes", _("Exclusion rules")),
                )

                includes, excludes = profile.toincludeexclude()
                for attrib, label in sections:
                    if attrib == "includes":
                        section = includes
                    elif attrib == "excludes":
                        section = excludes
                    else:
                        section = getattr(profile, attrib)
                    if not section:
                        continue
                    lines += (minirst.subsection(label), "::\n\n")
                    lines += ("  %s\n" % entry for entry in sorted(section))
                    lines += ("\n",)

                textwidth = ui.configint("ui", "textwidth")
                termwidth = ui.termwidth() - 2
                if not (0 < textwidth <= termwidth):
                    textwidth = termwidth
                fm.plain(minirst.format("".join(lines), textwidth))

    if not ui.verbose:
        hintutil.trigger("sparse-explain-verbose", *profiles)

    return exitcode


@subcmd(
    "files",
    [
        (
            "r",
            "rev",
            "",
            _("show the files in the specified revision"),
            _("REV"),
        ),
    ]
    + commands.templateopts,
    _("[OPTION]... PROFILE [FILES]..."),
)
def _listfilessubcmd(ui, repo, profile: Optional[str], *files, **opts) -> int:
    """list all files included in a sparse profile

    If files are given to match, print the names of the files in the profile
    that match those patterns.

    """
    _abortifnotregularsparse(repo)

    rev = opts.get("rev", ".")
    try:
        raw = getrawprofile(repo, profile, rev)
    except KeyError:
        raise error.Abort(_("The profile %s was not found\n") % profile)

    config = readsparseconfig(repo, raw, profile)
    ctx = repo[rev]
    matcher = matchmod.intersectmatchers(
        matchmod.match(repo.root, repo.getcwd(), files),
        repo.sparsematch(ctx.hex(), includetemp=False, config=config),
    )

    exitcode = 1
    ui.pager("sparse listfiles")
    with ui.formatter("files", opts) as fm:
        for f in ctx.matches(matcher):
            fm.startitem()
            fm.data(abspath=f)
            fm.write("path", "%s\n", matcher.rel(f))
            exitcode = 0
    return exitcode


_common_config_opts: List[Tuple[str, str, bool, str]] = [
    ("f", "force", False, _("allow changing rules even with pending changes")),
]


def getcommonopts(opts) -> Dict[str, Any]:
    force = opts.get("force")
    return {"force": force}


@subcmd("reset", _common_config_opts + commands.templateopts)
def resetsubcmd(ui, repo, **opts) -> None:
    """disable all sparse profiles and convert to a full checkout"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, [], opts, reset=True, **commonopts)


@subcmd("disable|disableprofile", _common_config_opts, "[PROFILE]...")
def disableprofilesubcmd(ui, repo, *pats, **opts) -> None:
    """disable a sparse profile"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, disableprofile=True, **commonopts)


def normalizeprofile(repo, p):
    # We want a canonical path from root of repo. Check if given path is already
    # canonical or is relative from cwd. This also normalizes path separators.
    for maybebase in (repo.root, repo.getcwd()):
        try:
            norm = pathutil.canonpath(repo.root, maybebase, p)
        except Exception:
            continue
        if repo.wvfs.exists(norm):
            return norm

    return p


@subcmd("enable|enableprofile", _common_config_opts, "[PROFILE]...")
def enableprofilesubcmd(ui, repo, *pats, **opts) -> None:
    """enable a sparse profile"""
    _abortifnotregularsparse(repo)
    pats = [normalizeprofile(repo, p) for p in pats]
    _checknonexistingprofiles(ui, repo, pats)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, enableprofile=True, **commonopts)


@subcmd("switch|switchprofile", _common_config_opts, "[PROFILE]...")
def switchprofilesubcmd(ui, repo, *pats, **opts) -> None:
    """switch to another sparse profile

    Disables all other profiles and stops including and excluding any additional
    files you have previously included or excluded.
    """
    _abortifnotregularsparse(repo)
    _checknonexistingprofiles(ui, repo, pats)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, reset=True, enableprofile=True, **commonopts)


@subcmd("delete", _common_config_opts, "[RULE]...")
def deletesubcmd(ui, repo, *pats, **opts) -> None:
    """delete an include or exclude rule (DEPRECATED)"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, delete=True, **commonopts)


@subcmd("exclude", _common_config_opts, "[RULE]...")
def excludesubcmd(ui, repo, *pats, **opts) -> None:
    """exclude some additional files"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, exclude=True, **commonopts)


@subcmd("unexclude", _common_config_opts, "[RULE]...")
def unexcludesubcmd(ui, repo, *pats, **opts) -> None:
    """stop excluding some additional files"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, unexclude=True, **commonopts)


@subcmd("include", _common_config_opts, "[RULE]...")
def includesubcmd(ui, repo, *pats, **opts) -> None:
    """include some additional files"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, include=True, **commonopts)


@subcmd("uninclude", _common_config_opts, "[RULE]...")
def unincludesubcmd(ui, repo, *pats, **opts) -> None:
    """stop including some additional files"""
    _abortifnotregularsparse(repo)
    commonopts = getcommonopts(opts)
    _config(ui, repo, pats, opts, uninclude=True, **commonopts)


for c in deletesubcmd, excludesubcmd, includesubcmd, unexcludesubcmd, unincludesubcmd:
    c.__doc__ += """\n
The effects of adding or deleting an include or exclude rule are applied
immediately. If applying the new rule would cause a file with pending
changes to be added or removed, the command will fail. Pass --force to
force a rule change even with pending changes (the changes on disk will
be preserved).
"""


@subcmd("importrules", _common_config_opts, _("[OPTION]... [FILE]..."))
def _importsubcmd(ui, repo, *pats, **opts) -> None:
    """import sparse profile rules

    Accepts a path to a file containing rules in the .hgsparse format.

    This allows you to add *include*, *exclude* and *enable* rules
    in bulk. Like the include, exclude and enable subcommands, the
    changes are applied immediately.

    """
    _import(ui, repo, pats, opts, force=opts.get("force"))


@subcmd("clear", _common_config_opts, _("[OPTION]..."))
def _clearsubcmd(ui, repo, *pats, **opts) -> None:
    """clear all extra files included or excluded

    Removes all extra include and exclude rules, without changing which
    profiles are enabled.

    """
    _clear(ui, repo, pats, force=opts.get("force"))


@subcmd("refresh", _common_config_opts, _("[OPTION]..."))
def _refreshsubcmd(ui, repo, *pats, **opts) -> None:
    """refresh the files on disk based on the enabled sparse profiles

    This is only necessary if .hg/sparse was changed by hand.
    """
    _abortifnotregularsparse(repo)

    force = opts.get("force")
    with repo.wlock():
        # Since we don't know the "original" sparse matcher, use the
        # always matcher so it checks everything.
        origmatcher = matchmod.always(repo.root, "")
        c = repo._refreshsparse(ui, repo.status(), origmatcher, force)
        fcounts = list(map(len, c))
        _verbose_output(ui, opts, 0, 0, 0, *fcounts)


@subcmd("cwd")
def _cwdsubcmd(ui, repo, *pats, **opts) -> None:
    """list all names in this directory

    The list includes any names that are excluded by the current sparse
    checkout; these are annotated with a hyphen ('-') before the name.

    """
    _cwdlist(repo)


def _config(
    ui,
    repo,
    pats,
    opts,
    include: bool = False,
    exclude: bool = False,
    reset: bool = False,
    delete: bool = False,
    uninclude: bool = False,
    unexclude: bool = False,
    enableprofile: bool = False,
    disableprofile: bool = False,
    force: bool = False,
) -> None:
    _abortifnotsparse(repo)

    """
    Perform a sparse config update. Only one of the kwargs may be specified.
    """
    wlock = repo.wlock()
    try:
        oldsparsematch = repo.sparsematch()

        if repo.localvfs.exists("sparse"):
            raw = repo.localvfs.readutf8("sparse")
            rawconfig = readsparseconfig(repo, raw)
            oldinclude, oldexclude = rawconfig.toincludeexclude()
            oldinclude = set(oldinclude)
            oldexclude = set(oldexclude)
            oldprofiles = set(rawconfig.profiles)
        else:
            oldinclude = set()
            oldexclude = set()
            oldprofiles = set()

        try:
            # Start from scratch for every update.
            if reset:
                newinclude = set()
                newexclude = set()
                newprofiles = set()
            else:
                newinclude = set(oldinclude)
                newexclude = set(oldexclude)
                newprofiles = set(oldprofiles)

            if any(os.path.isabs(pat) for pat in pats):
                err = _("paths cannot be absolute")
                raise error.Abort(err)

            # Edensparse doesn't support config-based adjustments
            if not _isedensparse(repo):
                adjustpats = False
                if include or exclude or delete or uninclude or unexclude:
                    if not ui.configbool("sparse", "includereporootpaths", False):
                        adjustpats = True
                if enableprofile or disableprofile:
                    if not ui.configbool("sparse", "enablereporootpaths", True):
                        adjustpats = True
                if adjustpats:
                    # supplied file patterns should be treated as relative
                    # to current working dir, so we need to convert them first
                    root, cwd = repo.root, repo.getcwd()
                    abspats = []
                    for kindpat in pats:
                        kind, pat = matchmod._patsplit(kindpat, None)
                        if kind in cwdrealtivepatkinds or kind is None:
                            kindpat = (kind + ":" if kind else "") + pathutil.canonpath(
                                root, cwd, pat
                            )
                        abspats.append(kindpat)
                    pats = abspats

            oldstatus = repo.status()
            if include:
                newexclude.difference_update(pats)
                newinclude.update(pats)
            elif exclude:
                newinclude.difference_update(pats)
                newexclude.update(pats)
            elif enableprofile:
                newprofiles.update(pats)
            elif disableprofile:
                newprofiles.difference_update(pats)
            elif uninclude:
                newinclude.difference_update(pats)
            elif unexclude:
                newexclude.difference_update(pats)
            elif delete:
                newinclude.difference_update(pats)
                newexclude.difference_update(pats)

            if _isedensparse(repo):
                # Aborts if a filter being enabled is not valid.
                for profile in newprofiles - oldprofiles:
                    readsparseprofile(repo, ".", profile, None, depth=1)

            repo.writesparseconfig(newinclude, newexclude, newprofiles)

            if _isedensparse(repo):
                repo._refreshsparse(ui, oldstatus, oldsparsematch, force)
            else:
                fcounts = list(
                    map(len, repo._refreshsparse(ui, oldstatus, oldsparsematch, force))
                )

                profilecount = len(newprofiles - oldprofiles) - len(
                    oldprofiles - newprofiles
                )
                includecount = len(newinclude - oldinclude) - len(
                    oldinclude - newinclude
                )
                excludecount = len(newexclude - oldexclude) - len(
                    oldexclude - newexclude
                )
                _verbose_output(
                    ui, opts, profilecount, includecount, excludecount, *fcounts
                )
        except Exception:
            repo.writesparseconfig(oldinclude, oldexclude, oldprofiles)
            raise
    finally:
        wlock.release()


def _checknonexistingprofiles(ui, repo, profiles) -> None:
    for p in profiles:
        try:
            repo.filectx(p, changeid=".").data()
        except error.ManifestLookupError:
            ui.warn(
                _(
                    "the profile '%s' does not exist in the "
                    "current commit, it will only take effect "
                    "when you check out a commit containing a "
                    "profile with that name\n"
                    f"(if the path is a typo, use '@prog@ {_getsparseflavor(repo)} disableprofile' to remove it)\n"
                )
                % p
            )


def _import(ui, repo, files, opts, force: bool = False) -> None:
    _abortifnotregularsparse(repo)

    with repo.wlock():
        # load union of current active profile
        revs = [
            repo.changelog.rev(node)
            for node in repo.dirstate.parents()
            if node != nullid
        ]

        # read current configuration
        raw = ""
        if repo.localvfs.exists("sparse"):
            raw = repo.localvfs.readutf8("sparse")
        orawconfig = readsparseconfig(repo, raw)
        oincludes, oexcludes = orawconfig.toincludeexclude()
        oprofiles = orawconfig.profiles
        includes, excludes, profiles = list(map(set, (oincludes, oexcludes, oprofiles)))

        # all active rules
        aincludes, aexcludes, aprofiles = set(), set(), set()
        for rev in revs:
            rsparseconfig = getsparsepatterns(repo, rev)
            rprofiles = rsparseconfig.allprofiles()
            rincludes, rexcludes = rsparseconfig.toincludeexclude()
            aincludes.update(rincludes)
            aexcludes.update(rexcludes)
            aprofiles.update(rprofiles)

        # import rules on top; only take in rules that are not yet
        # part of the active rules.
        changed = False
        for file in files:
            with util.posixfile(util.expandpath(file), "rb") as importfile:
                irawconfig = readsparseconfig(
                    repo, importfile.read().decode(), filename=file
                )
                iincludes, iexcludes = irawconfig.toincludeexclude()
                iprofiles = irawconfig.profiles
                oldsize = len(includes) + len(excludes) + len(profiles)
                includes.update(set(iincludes) - aincludes)
                excludes.update(set(iexcludes) - aexcludes)
                profiles.update(set(iprofiles) - aprofiles)
                if len(includes) + len(excludes) + len(profiles) > oldsize:
                    changed = True

        profilecount = includecount = excludecount = 0
        fcounts = (0, 0, 0)

        if changed:
            profilecount = len(profiles - aprofiles)
            includecount = len(includes - aincludes)
            excludecount = len(excludes - aexcludes)

            oldstatus = repo.status()
            oldsparsematch = repo.sparsematch()
            repo.writesparseconfig(includes, excludes, profiles)

            try:
                fcounts = list(
                    map(len, repo._refreshsparse(ui, oldstatus, oldsparsematch, force))
                )
            except Exception:
                repo.writesparseconfig(oincludes, oexcludes, oprofiles)
                raise

        _verbose_output(ui, opts, profilecount, includecount, excludecount, *fcounts)


def _clear(ui, repo, files, force: bool = False) -> None:
    _abortifnotregularsparse(repo)

    with repo.wlock():
        raw = ""
        if repo.localvfs.exists("sparse"):
            raw = repo.localvfs.readutf8("sparse")
        rawconfig = readsparseconfig(repo, raw)

        if rawconfig.lines:
            oldstatus = repo.status()
            oldsparsematch = repo.sparsematch()
            repo.writesparseconfig(set(), set(), rawconfig.profiles)
            repo._refreshsparse(ui, oldstatus, oldsparsematch, force)


def _verbose_output(
    ui, opts, profilecount, includecount, excludecount, added, dropped, lookup
) -> None:
    """Produce --verbose and templatable output

    This specifically enables -Tjson, providing machine-readable stats on how
    the sparse profile changed.

    """
    with ui.formatter("sparse", opts) as fm:
        fm.startitem()
        fm.condwrite(
            ui.verbose, "profiles_added", "Profile # change: %d\n", profilecount
        )
        fm.condwrite(
            ui.verbose,
            "include_rules_added",
            "Include rule # change: %d\n",
            includecount,
        )
        fm.condwrite(
            ui.verbose,
            "exclude_rules_added",
            "Exclude rule # change: %d\n",
            excludecount,
        )
        # In 'plain' verbose mode, mergemod.applyupdates already outputs what
        # files are added or removed outside of the templating formatter
        # framework. No point in repeating ourselves in that case.
        if not fm.isplain():
            fm.condwrite(ui.verbose, "files_added", "Files added: %d\n", added)
            fm.condwrite(ui.verbose, "files_dropped", "Files dropped: %d\n", dropped)
            fm.condwrite(
                ui.verbose, "files_conflicting", "Files conflicting: %d\n", lookup
            )


def _cwdlist(repo) -> None:
    """List the contents in the current directory. Annotate
    the files in the sparse profile.
    """
    _abortifnotregularsparse(repo)

    ctx = repo["."]
    mf = ctx.manifest()

    # Get the root of the repo so that we remove the content of
    # the root from the current working directory
    root = repo.root
    cwd = util.normpath(os.getcwd())
    cwd = os.path.relpath(cwd, root)
    cwd = "" if cwd == os.curdir else cwd + os.sep
    if cwd.startswith(os.pardir + os.sep):
        raise error.Abort(
            _("the current working directory should begin with the root %s") % root
        )

    matcher = matchmod.match(repo.root, repo.getcwd(), patterns=["path:" + cwd])
    files = mf.matches(matcher)

    sparsematch = repo.sparsematch(ctx.rev())
    checkedoutentries = set()
    allentries = set()
    cwdlength = len(cwd)

    for filepath in files:
        entryname = filepath[cwdlength:].partition(os.sep)[0]

        allentries.add(entryname)
        if sparsematch(filepath):
            checkedoutentries.add(entryname)

    ui = repo.ui
    for entry in sorted(allentries):
        marker = " " if entry in checkedoutentries else "-"
        ui.status("%s %s\n" % (marker, entry))


class forceincludematcher(matchmod.basematcher):
    """A matcher that returns true for any of the forced includes before testing
    against the actual matcher."""

    def __init__(self, matcher, includes):
        super(forceincludematcher, self).__init__(matcher._root, matcher._cwd)
        self._matcher = matcher
        self._includes = set(includes).union([""])

    def matchfn(self, value):
        return bool(value in self._includes or self._matcher(value))

    def __repr__(self):
        return "<forceincludematcher matcher=%r includes=%r>" % (
            self._matcher,
            self._includes,
        )

    def visitdir(self, dir):
        if any(True for path in self._includes if path.startswith(dir)):
            return True
        return self._matcher.visitdir(dir)

    def hash(self):
        sha1 = hashlib.sha1()
        sha1.update(_hashmatcher(self._matcher))
        for include in sorted(self._includes):
            sha1.update(include + "\0")
        return sha1.hexdigest()


class unionmatcher(matchmod.unionmatcher):
    def hash(self):
        sha1 = hashlib.sha1()
        for m in self._matchers:
            sha1.update(_hashmatcher(m))
        return sha1.hexdigest()


class ignorematcher(unionmatcher):
    def __init__(self, origignorematcher, negativesparsematcher):
        self._origignore = origignorematcher
        self._negativesparse = negativesparsematcher
        super().__init__([origignorematcher, negativesparsematcher])

    def explain(self, f):
        if self._origignore(f) or not self._negativesparse(f):
            return self._origignore.explain(f)
        else:
            return "%s is not in sparse profile" % f


class negatematcher(matchmod.basematcher):
    def __init__(self, matcher):
        super(negatematcher, self).__init__(matcher._root, matcher._cwd)
        self._matcher = matcher

    def __call__(self, value):
        return not self._matcher(value)

    def __repr__(self):
        return "<negatematcher matcher=%r>" % self._matcher

    def visitdir(self, dir):
        orig = self._matcher.visitdir(dir)
        if orig == "all":
            return False
        elif orig is False:
            return "all"
        else:
            return True

    def hash(self):
        sha1 = hashlib.sha1()
        sha1.update("negate")
        sha1.update(_hashmatcher(self._matcher))
        return sha1.hexdigest()


def _hashmatcher(matcher):
    if hasattr(matcher, "hash"):
        return matcher.hash()

    sha1 = hashlib.sha1()
    sha1.update(repr(matcher))
    return sha1.hexdigest()
