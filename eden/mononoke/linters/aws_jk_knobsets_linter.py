# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""
Lint rule: Mononoke code may only read JustKnobs from knobsets that the AWS
Mononoke server ships.

The AWS Mononoke server has no live JustKnobs. It reads a static file built by
configerator `source/cloud/helm/mononoke/server/configs/justknobs/justknobs.json.raw.cconf`,
which ships every `scm/mononoke*` knobset plus the few in OTHER_AWS_KNOBSETS.
A knob from any other knobset is missing on AWS, and reading a missing knob is
an error.

ast-grep finds every string literal that is a whole JK name, not only those at
`justknobs::` call sites: Mononoke code also picks knob names in `match` arms
and reads knobs inside macro bodies such as `tokio::select!`. Literals inside a
`#[cfg(test)]` module are skipped.

Output: newline-delimited JSON (LintMessage schema) on stdout.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from enum import Enum
from pathlib import Path
from typing import NamedTuple, Optional

LINTER_CODE = "MONONOKEAWSJK"

GENERATOR_PATH = (
    "configerator source/cloud/helm/mononoke/server/configs/justknobs/"
    "justknobs.json.raw.cconf"
)

# The generator ships every knobset under `justknobs/scm/` whose name starts
# with `mononoke` (not nested directories).
AUTO_SHIPPED_KNOBSET_RE = re.compile(r"^scm/mononoke[a-z0-9_]*$")

# Knobsets outside `scm/mononoke*` that the generator imports by name.
OTHER_AWS_KNOBSETS: frozenset[str] = frozenset({"scm/artillery"})

# Knobsets read only by code that never runs in the AWS Mononoke server.
NOT_ON_AWS_KNOBSETS: dict[str, str] = {
    # Hooks: every AWS shadow repo sets `all_hooks_bypassed`.
    "scm_secure/mononoke_hooks": "hooks",
    "ucp_security/iac_fbcode_hook": "hooks",
    # Separate binary, not deployed to AWS.
    "scm/metagit": "git server",
}

# A string literal that is exactly `"knobset/path:knob"`, outside any module
# whose attributes include `#[cfg(test)]`.
SG_RULE = r"""
id: jk-literal
language: rust
rule:
  kind: string_literal
  regex: '^"[a-z][a-z0-9_]*(/[a-z][a-z0-9_]*)+:[a-z][a-z0-9_:]*[a-z0-9_]"$'
  not:
    inside:
      kind: mod_item
      stopBy: end
      follows:
        kind: attribute_item
        regex: '^#\[cfg\(test\)\]$'
        stopBy:
          not:
            kind: attribute_item
"""

SG_TIMEOUT_SECONDS = 120

# Stay well below the kernel's argv limit when passing paths to ast-grep.
ARGV_BATCH_BYTES = 100_000


class LintSeverity(str, Enum):
    ERROR = "error"
    WARNING = "warning"
    ADVICE = "advice"
    DISABLED = "disabled"


class LintMessage(NamedTuple):
    path: str
    line: Optional[int]
    char: Optional[int]
    code: str
    severity: LintSeverity
    name: str
    original: Optional[str] = None
    replacement: Optional[str] = None
    description: Optional[str] = None
    bypassChangedLineFiltering: Optional[bool] = None


class KnobLiteral(NamedTuple):
    path: str
    line: int
    char: int
    name: str


def batches(paths: list[str]) -> list[list[str]]:
    out: list[list[str]] = [[]]
    used = 0
    for path in paths:
        if out[-1] and used + len(path) + 1 > ARGV_BATCH_BYTES:
            out.append([])
            used = 0
        out[-1].append(path)
        used += len(path) + 1
    return out


def knob_literals(sg_bundle: str, paths: list[str]) -> list[KnobLiteral]:
    """Knob-name literals in non-test code, via ast-grep."""
    found = []
    for batch in batches(paths):
        result = subprocess.run(
            [
                sg_bundle,
                "sg",
                "scan",
                "--inline-rules",
                SG_RULE,
                "--json",
                "--",
                *batch,
            ],
            capture_output=True,
            text=True,
            timeout=SG_TIMEOUT_SECONDS,
        )
        # 0: no matches, 1: matches; anything else is a failure.
        if result.returncode not in (0, 1):
            raise RuntimeError(f"ast-grep exited {result.returncode}: {result.stderr}")
        for match in json.loads(result.stdout or "[]"):
            start = match["range"]["start"]
            found.append(
                KnobLiteral(
                    path=match["file"],
                    line=start["line"] + 1,
                    char=start["column"] + 1,
                    name=match["text"][1:-1],
                )
            )
    return found


def allowed_on_aws(knobset: str) -> bool:
    return (
        AUTO_SHIPPED_KNOBSET_RE.match(knobset) is not None
        or knobset in OTHER_AWS_KNOBSETS
        or knobset in NOT_ON_AWS_KNOBSETS
    )


def findings(literals: list[KnobLiteral], severity: LintSeverity) -> list[LintMessage]:
    messages = []
    for lit in sorted(literals):
        knobset = lit.name.split(":", 1)[0]
        if allowed_on_aws(knobset):
            continue
        messages.append(
            LintMessage(
                path=lit.path,
                line=lit.line,
                char=lit.char,
                code=LINTER_CODE,
                severity=severity,
                name="jk-knobset-not-on-aws",
                description=(
                    f"Knobset '{knobset}' does not ship to the AWS Mononoke "
                    f"server, which gets only scm/mononoke* knobsets, so reading "
                    f"'{lit.name}' there is an error. Put the knob in an "
                    f"scm/mononoke* knobset. If the AWS server never runs this "
                    f"code, add the knobset to NOT_ON_AWS_KNOBSETS in this "
                    f"linter; if it must stay in this knobset, import it in "
                    f"{GENERATOR_PATH} and add it to OTHER_AWS_KNOBSETS."
                ),
            )
        )
    return messages


def emit(msg: LintMessage) -> None:
    print(json.dumps(msg._asdict()), flush=True)


def skip(path: str, name: str, description: str) -> None:
    emit(
        LintMessage(
            path=path,
            line=None,
            char=None,
            code=LINTER_CODE,
            severity=LintSeverity.DISABLED,
            name=name,
            description=description,
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description=(
            "Reject JustKnobs from knobsets the AWS Mononoke server does not ship."
        ),
        fromfile_prefix_chars="@",
    )
    parser.add_argument("filenames", nargs="+", help="Rust source files to check.")
    parser.add_argument(
        "--sg-bundle", required=True, help="Path to the ast-grep bundle binary."
    )
    parser.add_argument(
        "--severity",
        default="error",
        choices=["error", "warning", "advice"],
        help="Severity of the finding (default: %(default)s).",
    )
    args = parser.parse_args()

    if not Path(args.sg_bundle).is_file():
        skip(
            args.filenames[0],
            "ast-grep-not-available",
            "The ast-grep bundle is not available, so JustKnobs literals cannot be found.",
        )
        return
    try:
        literals = knob_literals(args.sg_bundle, args.filenames)
    except (RuntimeError, subprocess.TimeoutExpired, OSError, ValueError) as e:
        skip(args.filenames[0], "extraction-failed", f"ast-grep failed: {e}")
        return

    for msg in findings(literals, LintSeverity(args.severity)):
        emit(msg)


if __name__ == "__main__":
    main()
