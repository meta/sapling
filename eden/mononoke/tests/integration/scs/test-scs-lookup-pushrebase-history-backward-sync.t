# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

-- Init two Mononoke repos
  $ setconfig push.edenapi=true
  $ PUSHREBASE_REWRITE_DATES=1 create_large_small_repo
  Adding synced mapping entry
  $ start_large_small_repo
  Starting Mononoke server
  $ setup_configerator_configs
  $ enable_pushredirect 1
  $ init_local_large_small_clones

-- Start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

-- Start up the backsyncer in the background
  $ backsync_large_to_small_forever

-- Push to a large repo
  $ cd "$TESTTMP"/large-hg-client
  $ hg up -q master_bookmark

  $ mkdir -p smallrepofolder
  $ echo bla > smallrepofolder/bla
  $ hg ci -Aqm "Commit pushed to a large repo"
  $ hg push -r . --to master_bookmark -q

-- Wait to give backsyncer some time to catch up
  $ sleep 50
  $ flush_mononoke_bookmarks
  $ quiet mononoke_admin phases -R large-mon fetch -B master_bookmark
  $ quiet mononoke_admin phases -R small-mon fetch -B master_bookmark

-- Lookup over pushrebase in the large repo (commit -> pushrebase predecessor)
  $ lookup_large=$(scsc pushrebase-history -R large-mon -S bonsai,hg -B master_bookmark)
  $ echo "$lookup_large"
  repo=large-mon
  bonsai=* (glob)
  hg=* (glob)
  --
  repo=large-mon
  bonsai=* (glob)
  hg=* (glob)

-- Lookup over pushrebase in the small repo (commit -> sync predecessor -> pushrebase predecessor)
  $ lookup_small=$(scsc pushrebase-history -R small-mon -S bonsai,hg -B master_bookmark)
  $ echo "$lookup_small"
  repo=small-mon
  bonsai=* (glob)
  hg=* (glob)
  --
  repo=large-mon
  bonsai=* (glob)
  hg=* (glob)
  --
  repo=large-mon
  bonsai=* (glob)
  hg=* (glob)

-- Check that suffixes of lookup results match
  $ diff <(echo "$lookup_small") <(echo "$lookup_large")
  1,4d0
  < repo=small-mon
  < bonsai=* (glob)
  < hg=* (glob)
  < --
  [1]

-- Push to a small repo
  $ cd "$TESTTMP"/small-hg-client
  $ hg pull -q
  $ hg up -q master_bookmark

  $ echo foo > foo
  $ hg ci -Aqm "Commit pushed to a small repo"
  $ hg push -r . --to master_bookmark -q

-- Wait to give backsyncer some time to catch up
  $ sleep 50
  $ flush_mononoke_bookmarks
  $ quiet mononoke_admin phases -R large-mon fetch -B master_bookmark
  $ quiet mononoke_admin phases -R small-mon fetch -B master_bookmark

-- Lookup over pushrebase in the large repo (commit -> pushrebase predecessor -> sync predecessor)
  $ lookup_large=$(scsc pushrebase-history -R large-mon -S bonsai -B master_bookmark)
  $ echo "$lookup_large"
  repo=large-mon
  * (glob)
  --
  repo=large-mon
  * (glob)
  --
  repo=small-mon
  * (glob)

-- Lookup over pushrebase in the small repo (commit -> sync predecessor -> pushrebase predecessor -> sync predecessor)
  $ lookup_small=$(scsc pushrebase-history -R small-mon -S bonsai -B master_bookmark)
  $ echo "$lookup_small"
  repo=small-mon
  * (glob)
  --
  repo=large-mon
  * (glob)
  --
  repo=large-mon
  * (glob)
  --
  repo=small-mon
  * (glob)

-- Check that suffixes of lookup results match
  $ diff <(echo "$lookup_small") <(echo "$lookup_large")
  1,3d0
  < repo=small-mon
  < * (glob)
  < --
  [1]

-- Check synced_commit_mapping table data
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select large_repo_id, small_repo_id, source_repo from synced_commit_mapping order by mapping_id;"
  0|1|large
  0|1|large
  0|1|small
  0|1|large

-- Check that lookup fails for draft commit
  $ hg up -q master_bookmark
  $ echo bar > bar
  $ hg ci -Aqm "Bar"
  $ bar=$(hg log -r . -T "{node}")
  $ hg push --to master_bookmark -q
  $ scsc pushrebase-history -R small-mon -S bonsai --commit-id $bar
  *scsc: error: SourceControlService::commit_lookup_pushrebase_history failed with request_error(RequestError): RequestError: changeset * is not public, and only public commits could be pushrebased (glob)
  [1]
