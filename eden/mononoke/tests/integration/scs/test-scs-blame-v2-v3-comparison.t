# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# This test verifies that blame V3 (history manifest based) produces identical
# results to blame V2 (unode based) for all blame scenarios.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ ADDITIONAL_DERIVED_DATA="blame_v3" setup_common_config
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
  0ff73aedebab61e96d59d9fbe12b543b8f05b7af

  $ echo -e "a\nb\nd\ne\nf" > b
  $ commit B
  323afe77a1b1e632e54e8d5a683ba2cc8511f299

  $ echo -e "b\nc\nd\ne\nf" > b
  $ commit C
  482568559f8a410956c5c4be57f322c117f16733

  $ hg cp b d
  $ echo -e "b\nc\nx\ny\nz\nf" > d
  $ commit D
  7bc1e6fca1146ad5d5d4587bbfe3df8371999f15

  $ mkdir foo
  $ echo -e "anything" > foo/bar
  $ commit E
  fc739cde3faf3cad89cc36485e37ff55fe5a6e15

Push main branch:
  $ hg push -q --to master_bookmark --create

A more complicated case of a file coming from a merge and being renamed.
  $ hg checkout -q null
  $ echo -e "one\ntwo\nthree" > branch
  $ commit X
  85a8022da83605339db8480dac3ccf04764a1fd6

  $ echo other > other
  $ commit Y
  e33c2e40a0aa878c3fd80b91c562e7adae6bc761

  $ hg checkout -q "$COMMIT_E"
  $ hg merge "$COMMIT_Y"
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)
  $ hg mv branch numbers
  $ echo -e "zero\none\ntwo\nfour" > numbers
  $ commit Z
  8b297275c0e8803fba2ec465482e5bf0c7e774d4

Push merge commit:
  $ hg push -q --to master_bookmark
  $ cd ..

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

Blame V2 output — validate content and save for comparison:

Simple file with modifications (b at C):
  $ scsc blame --repo repo -i "$COMMIT_C" --path b --user --commit-number --no-commit-id | tee "$TESTTMP/v2_b_C.txt"
  test #0: b
  test #1: c
  test #0: d
  test #0: e
  test #0: f

File created via copy with modifications (d at D):
  $ scsc blame --repo repo -i "$COMMIT_D" --path d --user --commit-number --no-commit-id | tee "$TESTTMP/v2_d_D.txt"
  test #0: b
  test #1: c
  test #2: x
  test #2: y
  test #2: z
  test #0: f

File in a subdirectory (foo/bar at E):
  $ scsc blame --repo repo -i "$COMMIT_E" --path foo/bar --user --commit-number --no-commit-id | tee "$TESTTMP/v2_foobar_E.txt"
  test #0: anything

File from merge with rename (numbers at Z):
  $ scsc blame --repo repo -i "$COMMIT_Z" --path numbers --user --commit-number --no-commit-id --origin-path | tee "$TESTTMP/v2_numbers_Z.txt"
  test #1:      : zero
  test #0:branch: one
  test #0:branch: two
  test #1:      : four

Switch to blame V3 via JustKnob:
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:use_blame_v3": true
  >   }
  > }
  > EOF

Capture blame V3 output (should be identical to V2):
  $ scsc blame --repo repo -i "$COMMIT_C" --path b --user --commit-number --no-commit-id > "$TESTTMP/v3_b_C.txt"
  $ scsc blame --repo repo -i "$COMMIT_D" --path d --user --commit-number --no-commit-id > "$TESTTMP/v3_d_D.txt"
  $ scsc blame --repo repo -i "$COMMIT_E" --path foo/bar --user --commit-number --no-commit-id > "$TESTTMP/v3_foobar_E.txt"
  $ scsc blame --repo repo -i "$COMMIT_Z" --path numbers --user --commit-number --no-commit-id --origin-path > "$TESTTMP/v3_numbers_Z.txt"

Diff V2 vs V3 — must be identical:
  $ diff "$TESTTMP/v2_b_C.txt" "$TESTTMP/v3_b_C.txt"
  $ diff "$TESTTMP/v2_d_D.txt" "$TESTTMP/v3_d_D.txt"
  $ diff "$TESTTMP/v2_foobar_E.txt" "$TESTTMP/v3_foobar_E.txt"
  $ diff "$TESTTMP/v2_numbers_Z.txt" "$TESTTMP/v3_numbers_Z.txt"
