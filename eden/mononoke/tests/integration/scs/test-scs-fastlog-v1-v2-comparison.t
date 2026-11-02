# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# This test verifies that fastlog V2 (history manifest based) produces identical
# results to fastlog V1 (unode based) for all history scenarios.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ ADDITIONAL_DERIVED_DATA="fastlog_v2" setup_common_config
  $ setup_configerator_configs

Start mononoke server:
  $ start_and_wait_for_mononoke_server

Setup testing repo:
  $ hg clone -q mono:repo repo --noupdate
  $ cd repo

Helper for making commit:
  $ function commit() { # args: name, date
  >   hg commit -qAm $1 -d "$2"
  >   export COMMIT_$1="$(hg --debug id -i)"
  > }

Linear sequence modifying the same file:
  $ echo -e "a\nb\nc\nd\ne" > a
  $ commit A "2015-01-01"

  $ echo -e "a\nb\nd\ne\nf" > b
  $ commit B "2016-01-01"

  $ echo -e "b\nc\nd\ne\nf" > b
  $ commit C "2017-01-01"

File copy followed by modification:
  $ hg cp b d
  $ echo -e "b\nc\nx\ny\nz\nf" > d
  $ commit D "2018-01-01"

File in a subdirectory:
  $ mkdir foo
  $ echo -e "anything" > foo/bar
  $ commit E "2019-01-01"

Delete and re-add b to exercise history across deletions:
  $ hg rm b
  $ commit F "2020-01-01"

  $ echo -e "re-added b" > b
  $ commit G "2020-02-01"

Push main branch:
  $ hg push -q --to master_bookmark --create

Independent branch that will be merged in with a rename:
  $ hg checkout -q null
  $ echo -e "one\ntwo\nthree" > branch
  $ commit X "2021-01-01"

  $ echo other > other
  $ commit Y "2021-02-01"

Merge the independent branch back, renaming branch -> numbers:
  $ hg checkout -q "$COMMIT_G"
  $ hg merge -q "$COMMIT_Y"
  $ hg mv branch numbers
  $ echo -e "zero\none\ntwo\nfour" > numbers
  $ commit Z "2022-01-01"

Push merge commit:
  $ hg push -q --to master_bookmark
  $ cd ..

Start SCS server:
  $ start_and_wait_for_scs_server

Helper that extracts just the deterministic fields (Date/Author/Summary) so
we can validate fastlog V1 content independently of unstable commit hashes,
while still keeping the raw output for the byte-for-byte V1 vs V2 diff.
  $ function summarize() { grep -E '^(Date|Author|Summary):' "$1"; }

Fastlog V1 output -- validate content and save for comparison:

Path history of a simple file with modifications (b at C):
  $ scsc log --repo repo -i "$COMMIT_C" --path b > "$TESTTMP/v1_b_C.txt"
  $ summarize "$TESTTMP/v1_b_C.txt"
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  Date: 2016-01-01 00:00:00 +00:00
  Author: test
  Summary: B

Path history of a file created via copy (d at D):
  $ scsc log --repo repo -i "$COMMIT_D" --path d > "$TESTTMP/v1_d_D.txt"
  $ summarize "$TESTTMP/v1_d_D.txt"
  Date: 2018-01-01 00:00:00 +00:00
  Author: test
  Summary: D

Path history of a file in a subdirectory (foo/bar at E):
  $ scsc log --repo repo -i "$COMMIT_E" --path foo/bar > "$TESTTMP/v1_foobar_E.txt"
  $ summarize "$TESTTMP/v1_foobar_E.txt"
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E

Path history across deletions (b at G with the flag, walks back to B):
  $ scsc log --repo repo -i "$COMMIT_G" --path b --history-across-deletions --limit 5 > "$TESTTMP/v1_b_G_deletions.txt"
  $ summarize "$TESTTMP/v1_b_G_deletions.txt"
  Date: 2020-02-01 00:00:00 +00:00
  Author: test
  Summary: G
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  Date: 2016-01-01 00:00:00 +00:00
  Author: test
  Summary: B

Path history without --history-across-deletions stops at the re-add (b at G):
  $ scsc log --repo repo -i "$COMMIT_G" --path b --limit 5 > "$TESTTMP/v1_b_G.txt"
  $ summarize "$TESTTMP/v1_b_G.txt"
  Date: 2020-02-01 00:00:00 +00:00
  Author: test
  Summary: G

Path history of a file created by a rename in a merge commit (numbers at Z).
Fastlog reports only the renaming commit; it does not follow the branch ->
numbers rename introduced on the merge's second parent:
  $ scsc log --repo repo -i "$COMMIT_Z" --path numbers --limit 5 > "$TESTTMP/v1_numbers_Z.txt"
  $ summarize "$TESTTMP/v1_numbers_Z.txt"
  Date: 2022-01-01 00:00:00 +00:00
  Author: test
  Summary: Z

Full commit history at G (no path, linear chain of 7 commits):
  $ scsc log --repo repo -i "$COMMIT_G" --limit 10 > "$TESTTMP/v1_history_G.txt"
  $ summarize "$TESTTMP/v1_history_G.txt" | grep -c '^Summary:'
  7

Full commit history at Z (no path, includes merge ancestors X and Y):
  $ scsc log --repo repo -i "$COMMIT_Z" --limit 20 > "$TESTTMP/v1_history_Z.txt"
  $ summarize "$TESTTMP/v1_history_Z.txt" | grep -c '^Summary:'
  10

Linear commit history at Z (skips the merged-in side branch):
  $ scsc log --repo repo -i "$COMMIT_Z" --linear --limit 20 > "$TESTTMP/v1_linear_Z.txt"
  $ summarize "$TESTTMP/v1_linear_Z.txt" | grep -c '^Summary:'
  8

Path history with --skip:
  $ scsc log --repo repo -i "$COMMIT_C" --path b --skip 1 > "$TESTTMP/v1_b_C_skip1.txt"
  $ summarize "$TESTTMP/v1_b_C_skip1.txt"
  Date: 2016-01-01 00:00:00 +00:00
  Author: test
  Summary: B

Switch to fastlog V2 via JustKnob:
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:use_fastlog_v2": true
  >   }
  > }
  > EOF

Capture fastlog V2 output (should be identical to V1):
  $ scsc log --repo repo -i "$COMMIT_C" --path b > "$TESTTMP/v2_b_C.txt"
  $ scsc log --repo repo -i "$COMMIT_D" --path d > "$TESTTMP/v2_d_D.txt"
  $ scsc log --repo repo -i "$COMMIT_E" --path foo/bar > "$TESTTMP/v2_foobar_E.txt"
  $ scsc log --repo repo -i "$COMMIT_G" --path b --history-across-deletions --limit 5 > "$TESTTMP/v2_b_G_deletions.txt"
  $ scsc log --repo repo -i "$COMMIT_G" --path b --limit 5 > "$TESTTMP/v2_b_G.txt"
  $ scsc log --repo repo -i "$COMMIT_Z" --path numbers --limit 5 > "$TESTTMP/v2_numbers_Z.txt"
  $ scsc log --repo repo -i "$COMMIT_G" --limit 10 > "$TESTTMP/v2_history_G.txt"
  $ scsc log --repo repo -i "$COMMIT_Z" --limit 20 > "$TESTTMP/v2_history_Z.txt"
  $ scsc log --repo repo -i "$COMMIT_Z" --linear --limit 20 > "$TESTTMP/v2_linear_Z.txt"
  $ scsc log --repo repo -i "$COMMIT_C" --path b --skip 1 > "$TESTTMP/v2_b_C_skip1.txt"

Diff V1 vs V2 -- must be identical:
  $ diff "$TESTTMP/v1_b_C.txt" "$TESTTMP/v2_b_C.txt"
  $ diff "$TESTTMP/v1_d_D.txt" "$TESTTMP/v2_d_D.txt"
  $ diff "$TESTTMP/v1_foobar_E.txt" "$TESTTMP/v2_foobar_E.txt"
  $ diff "$TESTTMP/v1_b_G_deletions.txt" "$TESTTMP/v2_b_G_deletions.txt"
  $ diff "$TESTTMP/v1_b_G.txt" "$TESTTMP/v2_b_G.txt"
  $ diff "$TESTTMP/v1_numbers_Z.txt" "$TESTTMP/v2_numbers_Z.txt"
  $ diff "$TESTTMP/v1_history_G.txt" "$TESTTMP/v2_history_G.txt"
Full history across a merge returns the same set of commits in both versions,
but V1 and V2 may interleave the merged-in branch differently, so compare the
commit sets rather than the exact order:
  $ grep '^Commit:' "$TESTTMP/v1_history_Z.txt" | sort > "$TESTTMP/v1_history_Z_commits.txt"
  $ grep '^Commit:' "$TESTTMP/v2_history_Z.txt" | sort > "$TESTTMP/v2_history_Z_commits.txt"
  $ diff "$TESTTMP/v1_history_Z_commits.txt" "$TESTTMP/v2_history_Z_commits.txt"
  $ diff "$TESTTMP/v1_linear_Z.txt" "$TESTTMP/v2_linear_Z.txt"
  $ diff "$TESTTMP/v1_b_C_skip1.txt" "$TESTTMP/v2_b_C_skip1.txt"
