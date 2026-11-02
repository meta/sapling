#! /usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import argparse
import subprocess


def list_srcs(target_to_update) -> filter[str]:
    output = subprocess.check_output(
        ["buck", "query", 'labels(srcs, "%s")' % target_to_update], shell=False
    )
    return filter(None, output.decode("ascii").split("\n"))


def find_known_buck_targets() -> tuple[dict[str, str], dict[str, str]]:
    common_rust_folders = ["//eden/mononoke/...", "//common/rust/..."]
    deps = ["deps('{}')".format(folder) for folder in common_rust_folders]
    deps = " + ".join(deps)
    output = subprocess.check_output(
        ["buck", "query", "kind('rust_library', {})".format(deps)], shell=False
    )
    build_targets = filter(None, output.decode("ascii").split("\n"))
    external_targets = {}
    internal_targets = {}
    for target in build_targets:
        name = target.split(":")[-1]
        if target.startswith("//third-party"):
            external_targets[name] = target
        else:
            internal_targets[name] = target
    return external_targets, internal_targets


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target-to-update", required=True)
    args = parser.parse_args()

    external_targets, internal_targets = find_known_buck_targets()

    found_internal_targets = []
    found_external_targets = []

    def try_find_crate_deps(crate):
        if crate in internal_targets:
            found_internal_targets.append(internal_targets[crate])
            return True
        elif crate in external_targets:
            found_external_targets.append(crate)
            return True
        else:
            return False

    files = list_srcs(args.target_to_update)
    for file in files:
        with open(file) as f:
            for line in f.readlines():
                prefix = "extern crate"
                if line.startswith(prefix):
                    crate = line[len(prefix) :].strip().strip(";")
                    found = try_find_crate_deps(crate)
                    if not found:
                        found = try_find_crate_deps(crate.replace("_", "-"))
                        if not found:
                            print("unknown crate " + crate)

    indent = " " * 4
    print(indent + "deps = [")
    for target in sorted(found_internal_targets):
        formatted_target = indent * 2 + '"@%s"' % (target[1:],)
        print(formatted_target)
    print(indent + "],")

    print(indent + "external_deps = [")
    for target in sorted(found_external_targets):
        formatted_target = indent * 2 + '("rust-crates-io", None, "{}"),'.format(target)
        print(formatted_target)
    print(indent + "],")


if __name__ == "__main__":
    main()
