# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import absolute_import

import doctest
import os
import sys

# this is hack to make sure no escape characters are inserted into the output

if "TERM" in os.environ:
    del os.environ["TERM"]


def testmod(name, optionflags=0, testtarget=None):
    __import__(name)
    mod = sys.modules[name]
    if testtarget is not None:
        mod = getattr(mod, testtarget)

    # minimal copy of doctest.testmod()
    finder = doctest.DocTestFinder()
    checker = None
    runner = doctest.DocTestRunner(checker=checker, optionflags=optionflags)
    for test in finder.find(mod, name):
        runner.run(test)
    runner.summarize()


testmod("sapling.pathlog")
testmod("sapling.repl_record")
testmod("sapling.ext.github.archive_commit")
testmod("sapling.ext.github.gh_submit")
testmod("sapling.ext.github.github_gh_cli")
testmod("sapling.ext.github.github_repo_util")
testmod("sapling.ext.github.pr_parser")
testmod("sapling.ext.github.pull_request_arg")
testmod("sapling.ext.github.pull_request_body")
testmod("sapling.ext.github.templates")
testmod("sapling.ext.rage")
testmod("sapling.changegroup")
testmod("sapling.changelog")
testmod("sapling.cloneuri")
testmod("sapling.cmdutil")
testmod("sapling.color")
testmod("sapling.config")
testmod("sapling.context")
testmod("sapling.dagparser", optionflags=doctest.NORMALIZE_WHITESPACE)
testmod("sapling.dispatch")
testmod("sapling.drawdag")
testmod("sapling.encoding")
testmod("sapling.formatter")
testmod("sapling.git")
testmod("sapling.gituser")
testmod("sapling.hg")
testmod("sapling.match")
testmod("sapling.mdiff")
testmod("sapling.minirst")
testmod("sapling.mutation")
testmod("sapling.patch")
testmod("sapling.pathutil")
testmod("sapling.parser")
testmod("sapling.result")
testmod("sapling.revset")
testmod("sapling.revsetlang")
testmod("sapling.scmutil")
testmod("sapling.simplemerge")
testmod("sapling.smartset")
testmod("sapling.store")
testmod("sapling.templatefilters")
testmod("sapling.templater")
testmod("sapling.testing.ext.python")
testmod("sapling.testing.sh")
testmod("sapling.testing.t.diff")
testmod("sapling.testing.t.runtime")
testmod("sapling.testing.t.transform")
testmod("sapling.ui")
testmod("sapling.uiconfig")
testmod("sapling.url")
testmod("sapling.util")
testmod("sapling.util", testtarget="platform")
testmod("sapling.utils.iterutil")
testmod("sapling.ext.commitcloud.sync")
testmod("sapling.ext.fbcodereview")
testmod("sapling.ext.remotenames")
testmod("sapling.extensions")
