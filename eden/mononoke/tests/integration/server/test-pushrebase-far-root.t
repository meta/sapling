# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# A stack whose root is more than `recursion_limit` commits behind the
# bookmark is rejected with "Root is too far behind" unless the far-root
# manifest check is on, in which case the old part of the range is answered
# from the root and split-point content manifests and the recent part is
# walked as usual.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setconfig push.edenapi=true

The limit is far under the split distance (200) so the fixture stays small:
260 commits on master above the root.
  $ PUSHREBASE_RECURSION_LIMIT=64 setup_common_config "blob_files"
  $ cd $TESTTMP

  $ testtool_drawdag -R repo --no-default-files <<EOF
  > R-S0
  > # modify: R "old.txt" "line1\nline2\nline3\nline4\nline5\n"
  > # modify: S0 "s0.txt" "s0\n"
  > # bookmark: S0 master_bookmark
  > EOF
  R=* (glob)
  S0=* (glob)

  $ mononoke --scuba-log-file "$TESTTMP/scuba.json"
  $ wait_for_mononoke
  $ hg clone -q mono:repo repo2 --noupdate
  $ cd repo2
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > EOF
  $ ROOT=$(hg log -r 'master_bookmark^' -T '{node}')

Land the branch above the root: an early edit of old.txt and a file whose
name differs only in case from one the stack will add, then filler so the
root ends up 260 commits behind. Create the filler in one transaction;
lands are capped at 100 commits each.
  $ hg up -q master_bookmark
  $ printf 'EARLY\nline2\nline3\nline4\nline5\n' > old.txt
  $ echo early > Early.txt
  $ hg ci -Aqm 'branch: early edit'
  $ FILLER_ROOT=$(hg log -r . -T '{node}')
  $ {
  >   echo "$FILLER_ROOT-F001..F258"
  >   echo '# drawdag.defaultfiles=false'
  >   for i in $(seq 1 258); do printf '# F%03d/filler%d.txt = %d\n' "$i" "$i" "$i"; done
  > } > "$TESTTMP/filler.dag"
  $ hg debugdrawdag --config devel.all-warnings=false < "$TESTTMP/filler.dag"
  $ for rev in F090 F180 F258; do hg push -r $rev --to master_bookmark -q; done
  $ hg log -r master_bookmark -T '{node}\n' > $TESTTMP/master_before
  $ hg log -r "only(master_bookmark, $ROOT)" -T '.' | wc -c
  260

Derive content manifests and case-conflict skeleton manifests on master so
the root and split point have them; the derivation tailer keeps them this
way in production. Nothing else is derived during the pushes below.
  $ mononoke_admin derived-data -R repo derive -T content_manifests -T ccsm --all-bookmarks

A stack on the root, knob off: rejected as it always was.
  $ hg up -q $ROOT
  $ echo new > new.txt
  $ hg ci -Aqm 'stack: new file'
  $ hg push -r . --to master_bookmark 2>&1 | grep -E 'abort|updated remote'
  abort: Server error: invalid request: Pushrebase failed: Root is too far behind

Knob on (knobs are read at startup): the same stack lands on top of master.
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:pushrebase_far_root_manifest_conflicts": true
  >   }
  > }
  > EOF
  $ killandwait $MONONOKE_PID
  $ start_and_wait_for_mononoke_server --scuba-log-file "$TESTTMP/scuba.json"
  $ hg push -r . --to master_bookmark 2>&1 | grep -E 'abort|updated remote'
  updated remote bookmark master_bookmark to * (glob)
  $ hg log -r master_bookmark -T '{desc}\n'
  stack: new file
  $ hg log -r 'master_bookmark^' -T '{node}\n' | diff - $TESTTMP/master_before

A conflict with the old part: the branch edited old.txt before the split,
and merge resolution is off.
  $ hg up -q $ROOT
  $ printf 'line1\nline2\nline3\nline4\nLATE\n' > old.txt
  $ hg ci -qm 'stack: edit old.txt'
  $ hg push -r . --to master_bookmark 2>&1 | grep -E 'abort|updated remote'
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("old.txt"), right: MPath("old.txt") }]

With merge resolution on, the old-part overlap merges against the split
state and the result carries both edits.
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:pushrebase_enable_merge_resolution": true,
  >     "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes": false
  >   },
  >   "ints": {
  >     "scm/mononoke:pushrebase_max_merge_conflicts": 10,
  >     "scm/mononoke:pushrebase_max_merge_file_size": 10485760
  >   }
  > }
  > EOF
  $ killandwait $MONONOKE_PID
  $ start_and_wait_for_mononoke_server --scuba-log-file "$TESTTMP/scuba.json"
  $ hg push -r . --to master_bookmark 2>&1 | grep -E 'abort|updated remote'
  updated remote bookmark master_bookmark to * (glob)
  $ hg up -q master_bookmark
  $ cat old.txt
  EARLY
  line2
  line3
  line4
  LATE

A case conflict with the old part: the branch added Early.txt before the
split.
  $ hg up -q $ROOT
  $ echo e > early.txt
  $ hg ci -Aqm 'stack: early.txt'
  $ hg push -r . --to master_bookmark 2>&1 | grep -E 'abort|updated remote'
  abort: Server error: invalid request: Pushrebase failed: PotentialCaseConflict: the change this commit introduces at early.txt may conflict with other commits. Rebase and retry.

A conflict with the recent part: the branch adds a file at the tip, after
the split, and the stack adds the same name.
  $ hg up -q master_bookmark
  $ echo tip > recent.txt
  $ hg ci -Aqm 'branch: recent file'
  $ hg push -r . --to master_bookmark -q
  $ hg up -q $ROOT
  $ echo stack > recent.txt
  $ hg ci -Aqm 'stack: recent.txt'
  $ hg push -r . --to master_bookmark 2>&1 | grep -E 'abort|updated remote'
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("recent.txt"), right: MPath("recent.txt") }]

A stack with a merge commit falls back to the old error.
  $ hg up -q $ROOT
  $ echo l > left.txt
  $ hg ci -Aqm 'stack: left'
  $ LEFT=$(hg log -r . -T '{node}')
  $ hg up -q $ROOT
  $ echo r > right.txt
  $ hg ci -Aqm 'stack: right'
  $ hg merge -q $LEFT
  $ hg ci -qm 'stack: merge'
  $ hg push -r . --to master_bookmark 2>&1 | grep -cE 'updated remote'
  0
  [1]

The outcomes were logged (across the server restarts).
  $ jq -r 'select(.normal.log_tag == "pushrebase_far_root_manifest_check") | .normal.far_root_outcome' "$TESTTMP/scuba.json" | sort | uniq -c | awk '{print $2": "$1}'
  case_conflict: 1
  clean: 1
  conflict: 1
  merge_in_stack: 1
  merged: 1
  recent_conflict: 1
