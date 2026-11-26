# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Forward sync of a small repo commit whose synced parent is more than the
# large repo's pushrebase `recursion_limit` commits behind the target bookmark.

  $ export LARGE_REPO_ID=0
  $ export SMALL_REPO_ID=1
  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

The far-root manifest check is on for both repos.
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:pushrebase_far_root_manifest_conflicts": true
  >   }
  > }
  > EOF

The limit is far under the split distance (200) so the fixture stays small.
  $ PUSHREBASE_RECURSION_LIMIT=64 XREPOSYNC=1 init_large_small_repo
  Adding synced mapping entry
  Starting Mononoke server

Land 260 commits on the large repo's master above the last synced commit, so
the next synced commit's root ends up 260 commits behind.
  $ {
  >   printf 'L_C'
  >   for i in $(seq -w 1 260); do printf -- '-F%s' "$i"; done
  >   echo
  >   echo "# exists: L_C $LARGE_MASTER_BONSAI"
  >   echo "# bookmark: F260 master_bookmark"
  > } > "$TESTTMP/filler.dag"
  $ quiet testtool_drawdag -R large-mon < "$TESTTMP/filler.dag"

Derive content manifests and case-conflict skeleton manifests on the large
repo's master, as the derivation tailer does in production.
  $ mononoke_admin derived-data -R large-mon derive -T content_manifests -T ccsm --all-bookmarks

A small repo commit on top of the last synced commit.
  $ quiet testtool_drawdag -R small-mon <<EOF
  > S_B-S_C
  > # exists: S_B $SMALL_MASTER_BONSAI
  > # message: S_C "small repo commit"
  > # modify: S_C small_file "small"
  > # bookmark: S_C master_bookmark
  > EOF

  $ mononoke_x_repo_sync 1 0 tail --catch-up-once |& grep "processing log entry"
  [INFO] processing log entry #* (glob)

The commit lands on top of the large repo's master.
  $ LARGE_MASTER=$(mononoke_admin bookmarks -R large-mon get master_bookmark)
  $ mononoke_admin changelog -R large-mon graph -i "$LARGE_MASTER" -M | sed -n '1,3p'
  o  message: small repo commit
  │
  o  message: F260

FIXME: pushrebase runs with no recursion limit, so it walks all 260 commits
between the root and master instead of taking the far-root manifest check.
  $ jq -r 'select(.normal.log_tag == "pushrebase_far_root_manifest_check") | .normal.far_root_outcome' "$TESTTMP/x_repo_sync_scuba_logs"
