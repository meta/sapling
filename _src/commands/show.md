---
sidebar_position: 37
---

## show
<!--
  @generated SignedSource<<e44ce6a5cdc70a345ccce0dc82bbdef4>>
  Run `./scripts/generate-command-markdown.py` to regenerate.
-->


**show commit in detail**

Show the commit message and contents for the specified commit. If no commit
is specified, shows the current commit.

The revision can be given positionally or via `-r/--rev`:

- `sl show REV [FILE]...` — first positional is the revision, the rest  are files.

- `sl show -r REV [FILE]...` — all positionals are files.

A bare `sl show FILE` does not work, because `FILE` is parsed as a
revision.

`sl show` behaves similarly to `sl log -vp -r REV [OPTION]... [FILE]...`, or
if called without a `REV`, `sl log -vp -r . [OPTION]...` Use
`sl log` for more powerful operations than supported by `sl show`.

## arguments
| shortname | fullname | default | description |
| - | - | - | - |
| | `--nodates`| | omit dates from diff headers (but keeps it in commit header)|
| | `--noprefix`| | omit a/ and b/ prefixes from filenames|
| | `--stat`| | output diffstat-style summary of changes|
| `-g`| `--git`| | use git extended diff format|
| `-U`| `--unified`| `3`| number of lines of diff context to show|
| `-r`| `--rev`| | show the specified revision|
| `-t`| `--mutation`| `false`| diff against the mutation predecessor|
| `-w`| `--ignore-all-space`| | ignore white space when comparing lines|
| `-b`| `--ignore-space-change`| | ignore changes in the amount of white space|
| `-B`| `--ignore-blank-lines`| | ignore changes whose lines are all blank|
| `-Z`| `--ignore-space-at-eol`| | ignore changes in whitespace at EOL|
| `-T`| `--template`| | display with template|
| `-I`| `--include`| | include files matching the given patterns|
| `-X`| `--exclude`| | exclude files matching the given patterns|
