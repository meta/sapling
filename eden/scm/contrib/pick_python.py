# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""
Pick a Python that is more likely to be able to build with setup.py.

Arguments: Binary name (ex. python3).
Stdout: Full path to a system python, or the first argument of the input.

This script does not need to be executed using a system Python.
"""

import ast
import os
import subprocess
import sys

EXE = ".exe" if os.name == "nt" else ""


def load_build_env():
    """Load build/env's as environment variables."""
    up = os.path.dirname
    hgdir = up(up(os.path.realpath(__file__)))
    envpath = os.path.join(hgdir, "build", "env")
    if not os.path.exists(envpath):
        return {}
    with open(envpath, "r") as f:
        return dict(l.split("=", 1) for l in f.read().splitlines() if "=" in l)


def main(args):
    os.environ.update(load_build_env())
    names = (
        list(filter(None, [os.getenv("PYTHON_SYS_EXECUTABLE")]))
        + [
            p + EXE
            for p in [
                # Cannot use python3.12+ because distutils was removed in python3.12.
                # https://github.com/facebook/sapling/issues/1032
                "python3.11",
                "python3.10",
                "python3.9",
                "python3.8",
                "python3",
            ]
        ]
        + args
    )
    dirs = os.environ.get("PATH").split(os.pathsep)
    for name in names:
        if os.path.isabs(name):
            paths = [name]
        else:
            paths = [os.path.join(d, name) for d in dirs]
        for path in paths:
            if does_python_look_good(path):
                if os.name == "nt":
                    # This is a workaround for an issue with make.exe on Windows.
                    # On some of our Makefile targets (e.g., oss, oss-install) we set up environment variables.
                    # If environment variables are not set, backward slashes will be interpreted as such.
                    # e.g., having a make target that runs something like `FOO=bar echo c:\baz`
                    # will print `c:baz`, whereas having a target like `echo c:\baz` will print
                    # `c:\baz`.
                    path = path.replace("\\", "/")
                print(path)
                return

    # Fallback
    sys.stderr.write("warning: cannot find a proper Python\n")
    sys.stdout.write(names[0])


def does_python_look_good(path):
    if not os.path.isfile(path):
        return False
    try:
        cfg = ast.literal_eval(
            subprocess.check_output(
                [path, "-c", "import sysconfig;print(sysconfig.get_config_vars())"]
            ).decode("utf-8")
        )
        cflags = cfg.get("CFLAGS") or ""
        if "-nostdinc" in cflags.split():
            sys.stderr.write("%s: ignored, lack of C stdlib\n" % path)
            return False
        includepy = cfg["INCLUDEPY"]
        if not os.path.exists(os.path.join(includepy, "Python.h")):
            sys.stderr.write(
                "%s: ignored, missing Python.h in %s\n" % (path, includepy)
            )
            return False
        realpath = subprocess.check_output(
            [path, "-c", "import sys;print(sys.executable)"]
        )
        if b"fbprojects" in realpath:
            sys.stderr.write(
                "%s: ignored, avoid using the fb python for non-fb builds\n" % path
            )
            return False
        return True
    except Exception:
        return False


if __name__ == "__main__":
    code = main(sys.argv[1:]) or 0
    sys.stderr.flush()
    sys.stdout.flush()
    sys.exit(code)
