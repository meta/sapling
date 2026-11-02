# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

Setup config repo:
  $ cd "$TESTTMP"
  $ INFINITEPUSH_ALLOW_WRITES=true \
  >   INFINITEPUSH_NAMESPACE_REGEX='^scratch/.+$' \
  >   ALLOW_CHANGE_XREPO_MAPPING_EXTRA=true \
  >   create_large_small_repo
  Adding synced mapping entry
  $ start_large_small_repo
  Starting Mononoke server
  $ setup_configerator_configs
  $ enable_pushredirect 1
  $ init_local_large_small_clones
  $ cd "$TESTTMP/small-hg-client"
  $ enable pushrebase commitcloud
  $ setconfig infinitepush.server=false infinitepush.branchpattern="re:scratch/.+"
  $ cd "$TESTTMP/large-hg-client"
  $ enable pushrebase commitcloud
  $ setconfig infinitepush.server=false infinitepush.branchpattern="re:scratch/.+"

start SCS server and mononoke
  $ start_and_wait_for_scs_server

make some simple requests that we can use to check scuba logging

-- sync a commit which changes the mapping used to rewrite a commit
  $ update_commit_sync_map_first_option

  $ cd "$TESTTMP/large-hg-client"
  $ hg up -q master_bookmark
  $ hg mv -q smallrepofolder smallrepofolder_after
  $ hg ci -Aqm "commit which changes the mapping" --extra "change-xrepo-mapping-to-version=new_version"
  $ echo new_content > smallrepofolder_after/file.txt
  $ hg ci -Aqm 'commit after mapping change'
  $ hg push -qr . --to scratch/mapping_change --create

  $ SMALL_REPO_COMMIT="$(scsc xrepo-lookup \
  > --source-repo=large-mon \
  > --target-repo=small-mon \
  > --bookmark=scratch/mapping_change)"
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -qr "$SMALL_REPO_COMMIT"
  $ hg up -q "$SMALL_REPO_COMMIT"
  $ cat ./file.txt
  new_content
  $ mononoke_admin cross-repo --source-repo-id 0 --target-repo-id 1 map -B scratch/mapping_change
  RewrittenAs([(ChangesetId(Blake2(8a56d2598f3c31b87eef036bc618f9b449eaebaa4fd441c1fc48a98d7fdeea03)), CommitSyncConfigVersion("new_version"))])

Sync a commit which changes the mapping but doesn't touch the small repo otherwise.
-- NOTE: this is broken now because of mapping change commit being omitted in small repo
-- when there's no file changes that affect it.
  $ cd "$TESTTMP/large-hg-client"
  $ hg up -q master_bookmark
  $ hg rm -q smallrepofolder
  $ hg ci -Aqm "commit which empties the small repo"
  $ hg push -qr . --to master_bookmark
  $ echo new_content > new_file
-- We do set failpushrebase for mapping-changing commits requiring the local rebase.
  $ hg ci -Aqm "commit which changes the mapping" --extra "change-xrepo-mapping-to-version=new_version" --extra "failpushrebase=1"
  $ hg push -qr . --to master_bookmark
-- Force backsync as there's no background backsync running
  $ backsync_large_to_small 2>&1 | grep "syncing bookmark"
  * syncing bookmark master_bookmark to Some(ChangesetId(Blake2(*))) (glob)
  * syncing bookmark master_bookmark to Some(ChangesetId(Blake2(*))) (glob)
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -q
  $ hg update -q master_bookmark
  $ echo new_content > file-new.txt
  $ hg ci -Aqm 'commit after mapping change'
  $ hg push -qr . --to master_bookmark
