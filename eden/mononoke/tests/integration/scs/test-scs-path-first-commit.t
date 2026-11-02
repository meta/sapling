# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ setup_common_config
  $ setup_configerator_configs
  $ start_and_wait_for_mononoke_server
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ hginit_treemanifest repo
  $ cd repo

Helper for making commit:
  $ function commit() { # the arg is used both for commit message and variable name
  >   hg commit -qAm $1 -d $2 # create commit with a date
  >   export COMMIT_$1="$(hg --debug id -i)" # save hash to variable
  > }

Build a history where "foo" is added, modified, deleted, then re-added, and
"bar" is added once and never touched again.

  $ echo "1" > foo
  $ echo "1" > bar
  $ commit A "2020-01-01"

  $ echo "2" > foo
  $ commit B "2020-01-02"

  $ hg rm foo
  $ commit C "2020-01-03"

  $ echo "3" > foo
  $ commit D "2020-01-04"

start SCS server
  $ start_and_wait_for_scs_server

configure repo to push to mononoke
  $ cat >> .hg/hgrc <<EOF
  > [paths]
  > default = mono:repo
  > EOF

import testing repo to mononoke
  $ hg push --to master_bookmark --create -r "$COMMIT_D" -q

The first commit of "bar" is its original (and only) introduction in A.
(Fastlog is derived on demand by the first query below.)
  $ test "$(scsc path-first-commit --repo repo -i "$COMMIT_D" --path bar -S hg)" = "$COMMIT_A" && echo "bar first == A"
  bar first == A

"foo" is reported from its current contiguous history, i.e. the commit that
re-added it (D) after it was deleted in C (deletions are not followed).
  $ test "$(scsc path-first-commit --repo repo -i "$COMMIT_D" --path foo -S hg)" = "$COMMIT_D" && echo "foo first == D"
  foo first == D

JSON output exposes the resolved id under "ids".
  $ test "$(scsc --json path-first-commit --repo repo -i "$COMMIT_D" --path bar -S hg | jq -r .ids.hg)" = "$COMMIT_A" && echo "json bar == A"
  json bar == A

A path that has never existed is rejected with a clear error.
  $ scsc path-first-commit --repo repo -i "$COMMIT_D" --path nonexistent -S hg 2>&1 | grep -q "does not exist" && echo "missing path errors"
  missing path errors
