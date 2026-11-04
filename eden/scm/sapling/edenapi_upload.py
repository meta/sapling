# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.


import bindings

from . import error, mutation, node as nodemod
from .i18n import _, _n

TOKEN_KEY = "token"
INDEX_KEY = "index"


def checkcapable(repo):
    """Check that edenapi endpoint can be reached"""
    repo.edenapi.capabilities()


def _filtercommits(repo, nodes):
    """Returns list of missing commits"""
    try:
        with repo.ui.timesection("http.edenapi.upload_filter_commits"):
            stream = repo.edenapi.commitknown(nodes)
            return [
                item["hgid"] for item in stream if item["known"].get("Ok") is not True
            ]
    except (error.UncategorizedNativeError, error.HttpError) as e:
        raise error.Abort(e)


def _filteruploaded(repo, files, trees):
    """Returns list of missing blobs and trees"""
    try:
        with repo.ui.timesection("http.edenapi.upload_lookup"):
            stream = repo.edenapi.lookup_filenodes_and_trees(
                [fctx.filenode() for fctx in files],
                [tree[0] for tree in trees],
            )

            results = list(stream)
            blobslen = len(files)

            foundindicesblobs = {
                idx for idx, token in results if "HgFilenodeId" in token["data"]["id"]
            }
            foundindicestrees = {
                idx - blobslen
                for idx, token in results
                if "HgTreeId" in token["data"]["id"]
            }

            missingfiles = [
                fctx
                for index, fctx in enumerate(files)
                if index not in foundindicesblobs
            ]
            missingtrees = [
                tree
                for index, tree in enumerate(trees)
                if index not in foundindicestrees
            ]

            return missingfiles, missingtrees
    except (error.UncategorizedNativeError, error.HttpError) as e:
        raise error.Abort(e)


def _uploadfilenodes(repo, fctxs):
    """Upload file content and filenodes"""
    if not fctxs:
        return

    if not hasattr(repo.fileslog, "metadatastore"):
        raise error.Abort(
            _("file upload is not supported for %s repositories")
            % repo.storage_format()
        )

    # Batch fetch file parents in case we don't have them locally.
    repo.fileslog.metadatastore.prefetch(
        [(f.path(), f.filenode()) for f in fctxs], length=1
    )

    keys = []
    for fctx in fctxs:
        p1, p2 = fctx.filelog().parents(fctx.filenode())
        keys.append((fctx.path(), fctx.filenode(), p1, p2))
    dpack, _hpack = repo.fileslog.getmutablelocalpacks()
    try:
        with repo.ui.timesection("http.edenapi.upload_files"):
            use_sha1 = "sha1-only" in repo.edenapi.capabilities()
            stream, _stats = repo.edenapi.uploadfiles(dpack, keys, use_sha1)
            items = list(stream)
            repo.ui.status(
                _n(
                    "uploaded %d file\n",
                    "uploaded %d files\n",
                    len(items),
                )
                % len(items),
                component="edenapi",
            )

    except (error.UncategorizedNativeError, error.HttpError) as e:
        raise error.Abort(e)


def _uploadtrees(repo, trees):
    """Upload trees"""
    if not trees:
        return

    try:
        with repo.ui.timesection("http.edenapi.upload_trees"):
            stream, _stats = repo.edenapi.uploadtrees(trees)
            trees = list(stream)
            repo.ui.status(
                _n(
                    "uploaded %d tree\n",
                    "uploaded %d trees\n",
                    len(trees),
                )
                % len(trees),
                component="edenapi",
            )
    except (error.UncategorizedNativeError, error.HttpError) as e:
        raise error.Abort(e)


def _uploadchangesets(repo, changesets, mutations):
    """Upload changesets"""
    if not repo.ui.configbool("experimental", "upload-mutations", True):
        mutations = []

    uploaded, failed = [], []
    if not changesets:
        return uploaded, failed
    try:
        with repo.ui.timesection("http.edenapi.upload_changesets"):
            stream, _stats = repo.edenapi.uploadchangesets(changesets, mutations)
            foundids = {item["data"]["id"]["HgChangesetId"] for item in stream}
            repo.ui.status(
                _n(
                    "uploaded %d changeset\n",
                    "uploaded %d changesets\n",
                    len(foundids),
                )
                % len(foundids),
                component="edenapi",
            )
            for cs in changesets:
                if cs[0] in foundids:
                    uploaded.append(cs[0])
                else:
                    failed.append(cs[0])

            repo.ui.log(
                "edenapi_uploaded_changesets", edenapi_uploaded_changesets=len(uploaded)
            )
            return uploaded, failed
    except (error.UncategorizedNativeError, error.HttpError) as e:
        raise error.Abort(e)


def _getfiles(repo, nodes):
    """Get changed files"""
    toupload = set()
    for node in nodes.iterrev():
        ctx = repo[node]
        for f in ctx.files():
            if f not in ctx:
                continue
            fctx = ctx[f]
            toupload.add(fctx)
    return toupload


def _gettrees(repo, nodes):
    """Get changed trees"""
    treedepth = 1 << 15
    for node in nodes.iterrev():
        parentnodes = repo.changelog.dag.parentnames(node)
        mfnode = repo.changelog.changelogrevision(node).manifest
        basemfnodes = [
            repo.changelog.changelogrevision(p).manifest for p in parentnodes
        ]
        difftrees = bindings.manifest.subdirdiff(
            repo.manifestlog.datastore, "", mfnode, basemfnodes, treedepth
        )
        for subdir, treenode, treetext, p1, p2 in difftrees:
            yield treenode, p1, p2, treetext


def filetypefromfile(f):
    if f.isexec():
        return "Executable"
    elif f.islink():
        return "Symlink"
    else:
        return "Regular"


def parentsfromctx(ctx):
    p1 = ctx.p1().node()
    p2 = ctx.p2().node()
    if p1 != nodemod.nullid and p2 != nodemod.nullid:
        return (p1, p2)
    elif p1 != nodemod.nullid:
        return p1
    else:
        return None


def uploadhgchangesets(repo, nodes, force=False, skipknowncheck=False):
    """Upload list of nodes via EdenApi Uploads protocol

    EdenApi Uploads API consists of the following:

        * Endpoint for lookup any type of data (file contents, hg filenodes,  hg treemanifests, hg commits).
        * Endpoint for upload file contents.
        * Endpoint for upload hg filenodes.
        * Endpoint for upload hg treemanifest.
        * Endpoint for upload hg commits & mutation information.

    The upload process is split into several stages:

        * Check and skip commits that have been already uploaded building ``uploadcommitqueue``.
        * Check and skip hg filenodes that have been already uploaded building ``uploadblobqueue``.
        * Check and skip hg trees that have been already uploaded building ``uploadtreesqueue``.
        * Calculate ContentIds hashes and upload all file contents for the ``uploadblobqueue``
          but skipping already uploaded content ids first (this step also deduplicates content ids
          if they are the same for some filenodes). See edenapi.uploadfiles.
        * Upload hg filenodes (``uploadblobqueue``).
        * Upload hg trees (``uploadtreesqueue``).
        * Finally, upload hg changesets and hg mutation information (``uploadcommitqueue``).

    If ``force`` is True (the default is False) the lookup check isn't performed prior to upload for commits, filenodes and trees.
    It will be still performed for file contents.

    If ``skipknowncheck`` is True (the default is False) the lookup check isn't performed to filter out already uploaded commits.
    Assumed it is known already that they are missing on the server.

    Returns newly uploaded nodes and failed nodes.
    """

    # Build a queue of commits to upload
    uploadcommitqueue = (
        nodes if (force or skipknowncheck) else _filtercommits(repo, nodes)
    )

    if not uploadcommitqueue:
        # No commits to upload
        return set(), set()

    repo.ui.status(
        _n(
            "queue %d commit for upload\n",
            "queue %d commits for upload\n",
            len(uploadcommitqueue),
        )
        % len(uploadcommitqueue),
        component="edenapi",
    )

    # Sort uploadcommitqueue in topological order (use iterrev() to iterate from parents to children)
    uploadcommitqueue = repo.changelog.dag.sort(uploadcommitqueue)

    # Build a queue of missing filenodes to upload
    files = list(_getfiles(repo, uploadcommitqueue))

    # Build a queue of missing trees to upload
    trees = list(_gettrees(repo, uploadcommitqueue))

    uploadblobqueue, uploadtreesqueue = (
        (files, trees) if force else _filteruploaded(repo, files, trees)
    )

    repo.ui.status(
        _n(
            "queue %d file for upload\n",
            "queue %d files for upload\n",
            len(uploadblobqueue),
        )
        % len(uploadblobqueue),
        component="edenapi",
    )

    # Upload missing files and filenodes for the selected set of filenodes
    _uploadfilenodes(repo, uploadblobqueue)

    repo.ui.status(
        _n(
            "queue %d tree for upload\n",
            "queue %d trees for upload\n",
            len(uploadtreesqueue),
        )
        % len(uploadtreesqueue),
        component="edenapi",
    )

    # Upload missing trees
    _uploadtrees(repo, uploadtreesqueue)

    # Uploading changesets
    changesets = []
    for node in uploadcommitqueue.iterrev():
        repo.ui.note(
            _("uploading commit '%s'...\n") % nodemod.hex(node), component="edenapi"
        )
        ctx = repo[node]
        extras = [
            {"key": key.encode(), "value": value.encode()}
            for key, value in ctx.extra().items()
            if key != "branch"
        ]
        (time, timezone) = ctx.date()
        changesets.append(
            (
                node,
                {
                    "parents": parentsfromctx(ctx),
                    "manifestid": ctx.manifestnode(),
                    "user": ctx.user().encode(),
                    "time": int(time),
                    "tz": timezone,
                    "extras": extras,
                    "files": ctx.changeset().files,  # "files" should the filelist in the commit text
                    "message": ctx.description().encode(),
                },
            )
        )

    mutations = mutation.entriesfornodes(repo, uploadcommitqueue)
    mutations = [
        {
            "successor": mut.succ(),
            "predecessors": mut.preds(),
            "split": mut.split(),
            "op": mut.op(),
            "user": mut.user().encode(),
            "time": mut.time(),
            "tz": mut.tz(),
            "extras": [{"key": key, "value": value} for key, value in mut.extra()],
        }
        for mut in mutations
    ]

    return _uploadchangesets(repo, changesets, mutations)
