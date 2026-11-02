#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import argparse
import os
import subprocess
import tempfile
from typing import List


# Create the parser
parser = argparse.ArgumentParser(
    description="""Creates a homebrew bottle for the specified architecture

Also downloads additional brew bottles as required.
"""
)

parser.add_argument(
    "-t",
    "--target",
    default=None,
    type=str,
    help="Compilation target (e.g. aarch64-apple-darwin)",
    required=True,
)

parser.add_argument(
    "-r",
    "--release-version",
    default=None,
    type=str,
    help="Version for Sapling",
    required=True,
)

parser.add_argument(
    "-d",
    "--dotdir",
    default="git",
    type=str,
    help="Dot directory of the current repo (e.g. .git, .hg, .sl)",
    required=False,
)

parser.add_argument(
    "-o",
    "--formula-out",
    default="sapling.rb",
    type=str,
    help="Location of the resultant filled in formula",
    required=False,
)


def run_cmd(cmd: List[str]) -> str:
    return subprocess.check_output(cmd).decode("utf-8").rstrip()


def create_repo_tarball(dotdir):
    run_cmd(
        [
            "tar",
            "--exclude",
            f".{dotdir}/**",
            "--exclude",
            "sapling.tar.gz",
            "-czf",
            "./sapling.tar.gz",
            ".",
        ]
    )


def fill_in_formula_template(target, version, tmpdir, filled_formula_dir):
    brew_formula_rb = os.path.join(os.path.dirname(__file__), "brew_formula.rb")
    with open(brew_formula_rb, "r") as f:
        formula = f.read()
    sha256 = run_cmd(["shasum", "-a", "256", "./sapling.tar.gz"]).split()[0]
    formula = formula.replace(
        "%URL%",
        f"file://{os.path.join(os.path.abspath(os.getcwd()), 'sapling.tar.gz')}",
    )
    formula = formula.replace("%VERSION%", version)
    formula = formula.replace("%SHA256%", sha256)
    formula = formula.replace("%TMPDIR%", tmpdir)
    formula = formula.replace("%TARGET%", target)
    with open(filled_formula_dir, "w") as f:
        f.write(formula)


if __name__ == "__main__":
    args = parser.parse_args()
    tmpdir = tempfile.mkdtemp()
    create_repo_tarball(args.dotdir)
    fill_in_formula_template(
        args.target, args.release_version, tmpdir, args.formula_out
    )
