# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

`--from` and `--to` name a range of history by Bonsai or Hg hash: the commits
after `--from` up to and including `--to`, that is every ancestor of `--to`
that is not an ancestor of `--from`. Commits merged in are in it; drafts and
other branches are not, even when their SQL IDs fall between the two.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  >       F
  >       |
  >   W   M
  >    \ / \
  >     C   D
  >      \ /
  >       B   Y
  >       |   |
  >       A   X
  > # modify: A "file" "a"
  > # modify: B "file" "b"
  > # modify: C "file" "c"
  > # modify: D "other" "d"
  > # modify: M "file" "m"
  > # modify: F "file" "f"
  > # modify: W "file" "w"
  > # modify: X "file" "x"
  > # modify: Y "file" "y"
  > # bookmark: F master_bookmark
  > # bookmark: Y other_bookmark
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)
  F=* (glob)
  M=* (glob)
  W=* (glob)
  X=* (glob)
  Y=* (glob)

  $ strider() {
  >   render_mononoke_config
  >   GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4 \
  >     --print-visited-nodes \
  >     --family core \
  >     "$@" > strider.out 2> strider.err
  > }
  $ audited() {
  >   grep ' status=' strider.out | cut -d' ' -f1 \
  >   | sed -e "s/$A/A/" -e "s/$B/B/" -e "s/$C/C/" -e "s/$D/D/" -e "s/$F/F/" \
  >     -e "s/$M/M/" -e "s/$W/W/" -e "s/$X/X/" -e "s/$Y/Y/" | LC_ALL=C sort
  > }

From B to F: both sides of the merge, not B itself, and neither the draft W
nor the branch X-Y.

  $ strider --from "$B" --to "$F"
  $ audited
  C
  D
  F
  M

Before checking any commit, the log gives the read rate and how many commits
the range holds.

  $ grep -oE 'read_qps_per_store="?[0-9a-z]+' strider.err | tr -d '"'
  read_qps_per_store=5000
  $ grep 'Strider commit range' strider.err | grep -oE ' commits="?[0-9]+' | tr -d '" '
  commits=4

Hg hashes name the same commits.

  $ B_HG=$(mononoke_admin convert -R repo -f bonsai -t hg "$B" 2>/dev/null)
  $ F_HG=$(mononoke_admin convert -R repo -f bonsai -t hg "$F" 2>/dev/null)
  $ strider --from "$B_HG" --to "$F_HG"
  $ audited
  C
  D
  F
  M

When `--from` is not an ancestor of `--to`, the range goes back to where their
histories meet: W's history meets F's at C.

  $ strider --from "$W" --to "$F"
  $ audited
  D
  F
  M

A hash that names no commit in the repository is an error, and nothing is
audited.

  $ strider --from "$B" --to 0000000000000000000000000000000000000000000000000000000000000001
  [1]
  $ grep -o 'commit 0000000000000000000000000000000000000000000000000000000000000001 is not in the repository' strider.err
  commit 0000000000000000000000000000000000000000000000000000000000000001 is not in the repository
  $ audited
  $ strider --from 1111111111111111111111111111111111111111 --to "$F"
  [1]
  $ grep -c 'Failed to resolve --from' strider.err
  1

A range whose `--to` is `--from` or one of its ancestors is empty, which is an
error.

  $ strider --from "$F" --to "$B"
  [1]
  $ grep -o 'commit range is empty' strider.err
  commit range is empty

`--from` and `--to` go together, take only hashes, and cannot be combined
with `--chunk-size`. Like every command-line error, that exits 2.

  $ strider --from not-a-hash --to "$F"
  [2]
  $ grep -o "expected a Bonsai hash" strider.err
  expected a Bonsai hash

  $ strider --from "$B"
  [2]
  $ grep -o -- "--to <TO>" strider.err | head -1
  --to <TO>
  $ strider --from "$B" --to "$F" --chunk-size 2
  [2]
  $ grep -o "cannot be used with" strider.err
  cannot be used with

An SQL-ID range logs its first and last SQL ID and how many commits it holds.

  $ strider --chunk-size 2 --repo-lower-bound 2 --repo-upper-bound 4
  $ grep 'Strider SQL-ID range' strider.err | grep -oE '(first_sql_id|last_sql_id|commits)="?[0-9]+' | tr -d '"'
  first_sql_id=2
  last_sql_id=3
  commits=2
