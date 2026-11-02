# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

Measures how many SQL replica reads a single `commit_lookup_pushrebase_history`
request costs.

This needs a large repo with *two* small repos. `try_traverse_commit_sync_inner`
fans out over every small repo of the large repo, and a commit can be mapped into
at most one of them, so the cost scales with the number of small repos. A
one-small-repo fixture cannot show that, which is why this does not extend
test-scs-lookup-pushrebase-history-backward-sync.t.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

-- meg-mon (id 0, large), fbs-mon (id 1), ovr-mon (id 2).
-- PUSHREBASE_REWRITE_DATES makes every pushrebase actually rewrite the commit, so
-- the pushrebase mapping entry is non-trivial and the history traversal has
-- something to follow.
  $ export PUSHREBASE_REWRITE_DATES=1
  $ init_two_small_one_large_repo
  A=* (glob)
  A=* (glob)
  A=* (glob)

  $ FBSOURCE_MASTER_BONSAI=$(mononoke_admin bookmarks --repo-id 1 get master_bookmark)
  $ OVRSOURCE_MASTER_BONSAI=$(mononoke_admin bookmarks --repo-id 2 get master_bookmark)
  $ MEGAREPO_MERGE_BONSAI=$(mononoke_admin bookmarks --repo-id 0 get master_bookmark)

  $ add_synced_commit_mapping_entry 1 $FBSOURCE_MASTER_BONSAI 0 $MEGAREPO_MERGE_BONSAI TEST_VERSION_NAME
  $ add_synced_commit_mapping_entry 2 $OVRSOURCE_MASTER_BONSAI 0 $MEGAREPO_MERGE_BONSAI TEST_VERSION_NAME

  $ start_and_wait_for_mononoke_server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

-- Land a commit in the large repo so it has real pushrebase history.
  $ cd "$TESTTMP"
  $ hg clone -q mono:meg-mon meg-hg-cnt --noupdate
  $ cd "$TESTTMP/meg-hg-cnt"
  $ hg up -q master_bookmark
  $ echo file > file
  $ hg ci -Aqm "Commit pushed to the large repo"
  $ hg push -r . --to master_bookmark -q

  $ flush_mononoke_bookmarks
  $ quiet mononoke_admin phases -R meg-mon fetch -B master_bookmark

-- Resolve the commit outside the measured window: looking it up by bookmark inside
-- the measured call would fold a bookmark read into the count.
  $ BONSAI=$(mononoke_admin bookmarks --repo-id 0 get master_bookmark)

-- One serial request, against a freshly truncated log. The first call is the one
-- that matters: the commit sync cache stores positive results but never empty ones,
-- so a second call on the same commit would have a different count.
  $ truncate -s 0 "$TESTTMP/scuba.json"
  $ quiet scsc pushrebase-history -R meg-mon -S bonsai --commit-id "$BONSAI"

-- Replica reads attributed to the lookup itself. The method filter matters: one
-- `scsc pushrebase-history` invocation also issues resolve_commit_id and a
-- commit_lookup per history entry.
--
-- Each commit sync traversal resolves every small repo in one query, so this no
-- longer scales with the number of small repos.
  $ jq -r 'select(.normal.log_tag == "Request complete" and .normal.method == "commit_lookup_pushrebase_history") | .int.SqlReadsReplica' < "$TESTTMP/scuba.json"
  5
