---
sidebar_position: 23
---

## log
<!--
  @generated SignedSource<<f4d8290da6c472a39ced38a1d19d6d52>>
  Run `./scripts/generate-command-markdown.py` to regenerate.
-->


**show commit history**

Print the revision history of the specified files or the entire
project.

If no revision range is specified, the default revision range is the
current commit and all of its ancestors (`::.`).

The `--mutation` option links the revisions by mutation (amend,
rebase, fold, split and other rewrites) rather than by parent, and shows
patches and `--stat` against the commit each one was created from. Its
default revision range is the mutation history of the current commit
(`mutationhistory(.)`); use `-r &#x27;mutationhistory(REV)&#x27;` for another
commit&#x27;s, or `sl sl -t REV`. Commits that are not shown are skipped,
with a dashed line over them; `--show-missing` also shows the commits
not available locally.

File history is shown without following the rename or copy
history of files. To follow file history across renames and
copies, use the `-f/-- follow` option. If the `--follow`
option is used without a filename, only the ancestors or
descendants of the starting revision are shown.

By default, `sl log` prints the commit&#x27;s hash, non-trivial
parents, user, date, time, and the single-line summary. When the
`-v/--verbose` option is used, the list of changed files and
full commit message are shown.

With the `--graph` option, revisions are shown as an ASCII art
graph with the most recent commit at the top. The graph nodes
are depicted as follows: **o** is a commit, **@** is a working
directory parent, **x** is obsolete, and **v** is obsolete and diverged
(it was replaced by more than one visible version). Paths in the graph
are represented with **|**, **/** and so forth. **:** in place of a
**|** indicates one or more revisions in a path are omitted.

Use the `-L/--line-range FILE,M:N` option to follow the
history of lines from **M** to **N** in FILE. With the `-p/--
patch` option, only diff hunks affecting specified line range
will be shown. The `-L` option can be specified multiple
times and requires the `--follow` option. Currently, the line
range option is not compatible with `--graph` and is an
experimental feature.

`sl log --patch` may generate unexpected diff output for merge
commits, as it will only compare the merge commit against
its first parent. Also, only files different from BOTH parents
will appear in the **files:** section.

For performance reasons, `sl log FILE` may omit duplicate changes
made on branches and will not show removals or mode changes. To
see all such changes, use the `--removed` switch.

The history resulting from `-L/--line-range` options depends on
diff options: for instance, if white-spaces are ignored,
respective changes with only white-spaces in specified line range
will not be listed.

Some examples:

- commits with full descriptions and file lists:

```
sl log -v
```

- commits ancestral to the working directory:

```
sl log -f
```

- last 10 commits on the current branch:

```
sl log -l 10 -b .
```

- commits showing all modifications of a file, including removals:

```
sl log --removed file.c
```

- all commits that touch a directory, with diffs, excluding merges:

```
sl log -Mp lib/
```

- all revision numbers that match a keyword:

```
sl log -k bug --template "{rev}\n"
```

- the full hash identifier of the working directory parent:

```
sl log -r . --template "{node}\n"
```

- list available log templates:

```
sl log -T list
```

- check if a given commit is included in a bookmarked release:

```
sl log -r "a21ccf and ancestor(release_1.9)"
```

- find all commits by some user in a date range:

```
sl log -k alice -d "may 2008 to jul 2008"
```

- commits touching lines 13 to 23 for file.c:

```
sl log -L file.c,13:23
```

- commits touching lines 13 to 23 for file.c and lines 2 to 6 of  main.c with patch:

```
sl log -L file.c,13:23 -L main.c,2:6 -p
```

See `sl help dates` for a list of formats valid for `-d/--date`.

See `sl help revisions` for more about specifying and ordering
revisions.

See `sl help templates` for more about pre-packaged styles and
specifying custom templates. The default template used by the log
command can be customized via the `ui.logtemplate` configuration
setting.

Returns 0 on success.

## arguments
| shortname | fullname | default | description |
| - | - | - | - |
| `-f`| `--follow`| | follow changeset history, or file history across copies and renames|
| `-d`| `--date`| | show revisions matching date spec|
| `-C`| `--copies`| | show copied files|
| `-k`| `--keyword`| | do case-insensitive search for a given text|
| `-r`| `--rev`| | show the specified revision or revset|
| | `--removed`| | include revisions where files were removed|
| `-u`| `--user`| | revisions committed by user|
| `-P`| `--prune`| | do not display revision or any of its ancestors|
| `-p`| `--patch`| | show patch|
| `-g`| `--git`| | use git extended diff format|
| `-l`| `--limit`| | limit number of changes displayed|
| `-M`| `--no-merges`| | do not show merges|
| | `--stat`| | output diffstat-style summary of changes|
| `-G`| `--graph`| | show the revision DAG|
| `-T`| `--template`| | display with template|
| `-t`| `--mutation`| `false`| link the revisions by mutation instead of by parent|
| | `--show-missing`| `false`| with --mutation, show commits not available locally|
| `-I`| `--include`| | include files matching the given patterns|
| `-X`| `--exclude`| | exclude files matching the given patterns|
| | `--all`| | shows all changesets in the repo|
| | `--sparse`| | limit to changesets affecting the sparse checkout|
| | `--remote`| | show remote names even if hidden|
