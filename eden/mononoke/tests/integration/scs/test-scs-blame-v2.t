# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ setup_common_config
  $ setup_configerator_configs

Start mononoke server:
  $ start_and_wait_for_mononoke_server

Setup testing repo:
  $ hg clone -q mono:repo repo --noupdate
  $ cd repo

Helper for making commit:
  $ function commit() { # the arg is used both for commit message and variable name
  >   hg commit -qAm $1 # create commit
  >   export COMMIT_$1="$(hg --debug id -i)" # save hash to variable
  >   hg --debug id -i # print it too
  > }

Three simple commits:
  $ echo -e "a\nb\nc\nd\ne" > a
  $ commit A
  * (glob)

  $ echo -e "a\nb\nd\ne\nf" > b
  $ commit B
  * (glob)

  $ echo -e "b\nc\nd\ne\nf" > b
  $ commit C
  * (glob)

  $ hg cp b d
  $ echo -e "b\nc\nx\ny\nz\nf" > d
  $ commit D
  * (glob)

  $ mkdir foo
  $ echo -e "anything" > foo/bar
  $ commit E
  * (glob)

Push main branch:
  $ hg push -q --to master_bookmark --create

A more complicated case of a file coming from a merge and being renamed.
  $ hg checkout -q null
  $ echo -e "one\ntwo\nthree" > branch
  $ commit X
  * (glob)

  $ echo other > other
  $ commit Y
  * (glob)

  $ hg checkout -q "$COMMIT_E"
  $ hg merge "$COMMIT_Y"
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)
  $ hg mv branch numbers
  $ echo -e "zero\none\ntwo\nfour" > numbers
  $ commit Z
  * (glob)

Push merge commit:
  $ hg push -q --to master_bookmark
  $ cd ..

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

  $ scsc blame --repo repo -i "$COMMIT_C" --path b -n
  #0 * b (glob)
  #1 * c (glob)
  #0 * d (glob)
  #0 * e (glob)
  #0 * f (glob)

  $ scsc blame --repo repo -i "$COMMIT_C" --parent --path b
  * a (glob)
  * b (glob)
  * d (glob)
  * e (glob)
  * f (glob)

  $ scsc blame --repo repo -i "$COMMIT_D" --path d --commit-number --origin-path --origin-line-number --parent-line-range --line-number
  #0 *:      :b:2:1: b (glob)
  #1 *:    +3:b:2:2: c (glob)
  #2 *:[b]3-4: :3:3: x (glob)
  #2 *:[b]3-4: :4:4: y (glob)
  #2 *:[b]3-4: :5:5: z (glob)
  #0 *:      :b:5:6: f (glob)

  $ scsc blame --repo repo -i "$COMMIT_D" --path d --user --commit-number --no-commit-id
  test #0: b
  test #1: c
  test #2: x
  test #2: y
  test #2: z
  test #0: f

  $ scsc blame --repo repo -i "$COMMIT_E" --path foo/bar --user --commit-number --no-commit-id
  test #0: anything

  $ scsc blame --json --repo repo -i "$COMMIT_C" --path b | jq -S .
  [
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 0,
      "contents": "b",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 1,
      "origin_line": 2,
      "path": "b",
      "title": "B"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 1,
      "contents": "c",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 2,
      "origin_line": 2,
      "parent": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "parent_index": 0,
      "parent_range_length": 0,
      "parent_start_line": 3,
      "path": "b",
      "title": "C"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 0,
      "contents": "d",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 3,
      "origin_line": 3,
      "path": "b",
      "title": "B"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 0,
      "contents": "e",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 4,
      "origin_line": 4,
      "path": "b",
      "title": "B"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 0,
      "contents": "f",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 5,
      "origin_line": 5,
      "path": "b",
      "title": "B"
    }
  ]

The more complicated case.  Note that the blame origin and parent information
is correct even though the file came from the second parent and was renamed.

  $ scsc --json blame --repo repo -i "$COMMIT_Z" --path numbers | jq -S .
  [
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 1,
      "contents": "zero",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 1,
      "origin_line": 1,
      "parent": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "parent_index": 1,
      "parent_path": "branch",
      "parent_range_length": 0,
      "parent_start_line": 1,
      "path": "numbers",
      "title": "Z"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 0,
      "contents": "one",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 2,
      "origin_line": 1,
      "path": "branch",
      "title": "X"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 0,
      "contents": "two",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 3,
      "origin_line": 2,
      "path": "branch",
      "title": "X"
    },
    {
      "author": "test",
      "commit": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "commit_number": 1,
      "contents": "four",
      "datetime": "1970-01-01T00:00:00+00:00",
      "line": 4,
      "origin_line": 4,
      "parent": {
        "bonsai": "*", (glob)
        "hg": "*" (glob)
      },
      "parent_index": 1,
      "parent_path": "branch",
      "parent_range_length": 1,
      "parent_start_line": 3,
      "path": "numbers",
      "title": "Z"
    }
  ]

  $ scsc blame --repo repo -i "$COMMIT_Z" --path numbers -OPTlnou --title-width 5
  test #1 *:Z    : (^2)[branch]+1:      :1:1: zero (glob)
  test #0 *:X    :               :branch:1:2: one (glob)
  test #0 *:X    :               :branch:2:3: two (glob)
  test #1 *:Z    :(^2)[branch]3-3:      :4:4: four (glob)

  $ scsc blame --repo repo -i "$COMMIT_Z" --path /
  *eden/mononoke/clients/scsc/__scsc__/scsc: error: SourceControlService::commit_path_blame failed with request_error(RequestError): RequestError: Blame is not available for directories: MPath("") (glob)
  [1]

  $ scsc blame --repo repo -i "$COMMIT_Z" --path foo
  *eden/mononoke/clients/scsc/__scsc__/scsc: error: SourceControlService::commit_path_blame failed with request_error(RequestError): RequestError: Blame is not available for directories: MPath("foo") (glob)
  [1]

