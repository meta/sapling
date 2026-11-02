#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import argparse
import json
import os
import subprocess
import sys
from enum import Enum
from glob import iglob
from os.path import abspath, basename, dirname, join
from pathlib import Path
from sys import platform


class TestGroup(Enum):
    PASSING = "passing"
    TIMING_OUT = "timing_out"
    REQUIRING_SCS = "requiring_scs"
    FLAKY = "flaky"
    BROKEN = "broken"
    ALL = "all"

    def __str__(self):
        return self.value


def script_dir():
    return dirname(abspath(__file__))


def parse_args():
    parser = argparse.ArgumentParser(
        description="Run Mononoke integration tests from getdeps.py build"
    )
    subs = parser.add_subparsers()

    local_parser = subs.add_parser(
        "local",
        help=(
            "Command to run tests from the current checkout of repo assuming that"
            " the required dependencies were already installed by getdeps"
            " (getdeps.py build mononoke_integration)."
        ),
    )
    local_parser.set_defaults(func=local_cmd)

    getdeps_parser = subs.add_parser(
        "getdeps",
        help=(
            "Command that is invoked by getdeps, you probably don't want to call it"
            " directly."
        ),
    )
    getdeps_parser.set_defaults(func=getdeps_cmd)
    getdeps_parser.add_argument(
        "--generate_manifest",
        help="Generate manifest.json file in build directory and don't run tests",
        action="store_true",
    )

    for p in (local_parser, getdeps_parser):
        p.add_argument("getdeps_install_dir", help="Location of getdeps.py install dir")
        p.add_argument(
            "tests",
            nargs="*",
            help="Optional list of tests to run. If provided the --test-groups default is None",
        )
        p.add_argument(
            "-t",
            "--test-groups",
            type=TestGroup,
            nargs="*",
            choices=list(TestGroup),
            help=f"Choose groups of tests to run, default: [{TestGroup.PASSING}]",
        )
        p.add_argument(
            "-r",
            "--rerun-failed",
            action="store_true",
            help="Rerun failed tests based on '.testfailed' file",
        )
        p.add_argument(
            "--dry-run",
            action="store_true",
            help="Just print which tests will be run without running them",
        )
        p.add_argument(
            "--keep-tmpdir",
            action="store_true",
            help="Keep temporary directory after running tests",
        )
        p.add_argument(
            "-j",
            "--jobs",
            action="store",
            type=int,
            help="number of jobs to run in parallel",
        )

    return parser.parse_args()


def local_cmd(args):
    install_dir = args.getdeps_install_dir
    repo_root = dirname(dirname(dirname(dirname(script_dir()))))

    if not args.dry_run:
        prepare_manifest_deps(install_dir, repo_root)
    run_tests(args, join(install_dir, "../build/mononoke_integration"))


def getdeps_cmd(args):
    install_dir = args.getdeps_install_dir
    mononoke_repo_root = join(install_dir, "mononoke/source")

    if args.generate_manifest:
        prepare_manifest_deps(install_dir, mononoke_repo_root)
    else:
        run_tests(args, join(install_dir, "mononoke_integration"))


def prepare_manifest_deps(install_dir, mononoke_repo_root):
    build_dir = join(install_dir, "../build/mononoke_integration")
    manifest_deps_path = join(script_dir(), "manifest_deps")

    exec(
        "global OSS_DEPS; global MONONOKE_BINS; global EDENSCM_BINS; "
        + open(manifest_deps_path, "r").read()
    )

    py3exe = os.environ.get("PYTHON_SYS_EXECUTABLE", "python3")

    MANIFEST_DEPS = {}
    for k, v in OSS_DEPS.items():  # noqa: F821
        if v.startswith("//"):
            MANIFEST_DEPS[k] = join(mononoke_repo_root, v[2:])
        elif v.startswith("/"):
            installdep = join(install_dir, v[1:])
            print(f"Adding install dependency {installdep}")
            MANIFEST_DEPS[k] = installdep
        elif k == "BINARY_HGPYTHON":
            MANIFEST_DEPS[k] = py3exe
        else:
            MANIFEST_DEPS[k] = v
    for k, v in MONONOKE_BINS.items():  # noqa: F821
        MANIFEST_DEPS[k] = join(install_dir, "mononoke/bin", v)

    os.makedirs(build_dir, exist_ok=True)
    manifest_file = join(build_dir, "manifest.json")
    print(f"Writing test runner manifest to {manifest_file}")
    with open(manifest_file, "w") as f:
        f.write(json.dumps(MANIFEST_DEPS, sort_keys=True, indent=4))


def get_test_groups():
    test_groups = {
        TestGroup.TIMING_OUT: {
            "server/test-commitcloud-upload.t",
            "server/test-pushrebase-allow-casefolding.t",
            "server/test-pushrebase-ensure-ancestors-of.t",
            "server/test-pushrebase-git-mapping.t",
            "server/test-pushrebase-globalrevs.t",
        },
        TestGroup.FLAKY: {
            # abort: server responded 500 Internal Server Error for https://localhost:$LOCAL_PORT/edenapi/large_repo/commit/translate_id: {"message":"internal error: Validation of submodule expansion failed:
            "cross_repo/test-cross-repo-initial-import-gitsubmodules-expand-recursive.t",
            # intermittent hg cli core dumps
            "edenapi/test-edenapi-server-files.t",
            # warning: some filter configuration was not removed (found filter.lfs.clean)
            "gitimport/test-gitimport-lfs-enabled-dangling-pointer.t",
            # intermittent hg cli core dumps
            "megarepo/test-megarepo-catchup.t",
            # flaky internally and externally
            "mononoke_re_cas/test-mononoke-cas-sync-job-merge-commit-sync.t",
            # intermittenly outputs: 0000000000000000000000000000000000000000 for
            # hg debugsh -c 'ui.write("%s\n" % s.node.hex(repo["."].filectx("was_a_lively_fellow").getnodeinfo()[2]))'
            "server/test-pushrebase.t",
            # intermittent hg cli core dumps
            "test-packer-tuning-debug-info.t",
        },
        TestGroup.BROKEN: {
            # no live config reload in OSS
            "cross_repo/test-cross-repo-commit-sync-live-via-extra.t",
            # output differs in what looks like harmless way
            "cross_repo/test-cross-repo-initial-import-and-merge-into-existing-megarepo.t",
            # mononoke_hg_sync_loop fails with exit status 1, differs on: -  * successful sync of entries [6]* (glob)
            "mononoke_hg_sync/test-mononoke-hg-sync-job.t",
            # Differs on diff -w
            "mononoke_git_server/test-mononoke-git-server-filemode-delta-cycle.t",
            # Differs on diff at end
            "mononoke_git_server/test-mononoke-git-server-pull-with-tag.t",
            # Differs on jq count output
            "mononoke_git_server/test-mononoke-git-server-push-with-fb-product-log.t",
            # differs on: warning: remote HEAD refers to nonexistent ref, unable to checkout
            "mononoke_git_server/test-mononoke-git-server-clone-with-invalid-head.t",
            # tags are missing in OSS run: - tags/first_tag|032CD4DCE0406F1C1DD1362B6C3C9F9BDFA82F2FC5615E237A890BE4FE08B044
            "mononoke_git_server/test-mononoke-git-server-push-with-tags.t",
            # gives: Blob is missing: hgaugmentedmanifest.sha1.317229df66cfb6504b14a0818288e8a9b236a688"
            "mononoke_re_cas/test-mononoke-cas-sync-job-random.t",
            # missing indexedloghistorystore in cachepath on OSS
            "test-gettreepack.t",
            # output differs
            "test-infinitepush.t",
        },
    }

    if platform == "darwin":
        test_groups[TestGroup.BROKEN].update(
            {
                "test-pushrebase-block-public-casefolding.t",  # MacOS is case insensitive
            }
        )

    manual_groups = set()
    not_unique = set()
    for group in test_groups.values():
        not_unique.update(manual_groups & group)
        manual_groups.update(group)
    assert not not_unique, f"The test groups contain not unique tests: {not_unique}"

    search_from = script_dir()
    test_groups[TestGroup.ALL] = all_tests = {
        p.removeprefix(search_from + "/")
        for p in iglob(join(search_from, "**/*.t"), recursive=True)
    }

    not_existing = manual_groups - all_tests
    assert not not_existing, (
        f"The test groups contain not existing tests: {sorted(not_existing)} in {search_from}"
    )

    rest_groups = all_tests - manual_groups
    # The test-scs* tests use scsc which is not buildable via getdeps/cargo yet
    test_groups[TestGroup.REQUIRING_SCS] = requiring_scs = {
        t for t in rest_groups if "test-scs" in t
    }
    test_groups[TestGroup.PASSING] = rest_groups - requiring_scs

    return test_groups


def get_tests_to_run(tests, groups_to_run, rerun_failed, test_flag_root):
    tests_to_run = set()
    test_groups = get_test_groups()

    if rerun_failed:
        # Based on eden/scm/tests/run-tests.py
        mapping = {basename(t): t for t in test_groups[TestGroup.ALL]}
        for title in ("failed", "errored"):
            failed = Path(test_flag_root) / ".test{}".format(title)
            if failed.is_file():
                tests_to_run.update(
                    mapping.get(t, t) for t in failed.read_text().splitlines() if t
                )
    else:
        groups_to_run = set(
            groups_to_run
            or ([TestGroup.PASSING] if not (tests or rerun_failed) else [])
        )

        for group in groups_to_run:
            tests_to_run.update(test_groups[group])

        if tests:
            tests_to_run.update(p for p in tests or [])

    return tests_to_run


def run_tests(args, manifest_json_dir):
    manifest_json_path = join(manifest_json_dir, "manifest.json")
    with open(manifest_json_path) as json_file:
        manifest = json.load(json_file)
        test_flag_root = manifest["RUN_TESTS_LIBRARY"]

    tests_to_run = get_tests_to_run(
        args.tests, args.test_groups, args.rerun_failed, test_flag_root
    )

    env = dict(os.environ.items())
    env["NO_LOCAL_PATHS"] = "1"

    if args.dry_run:
        print("\n".join(tests_to_run))
    elif tests_to_run:
        cmd = [
            sys.executable,
            join(script_dir(), "integration_runner_real.py"),
            manifest_json_path,
        ]
        if args.keep_tmpdir:
            cmd.append("--keep-tmpdir")
        if args.jobs:
            cmd.append(f"--jobs={args.jobs}")
        sys.exit(subprocess.run(cmd + sorted(list(tests_to_run)), env=env).returncode)


if __name__ == "__main__":
    args = parse_args()
    args.func(args)
