# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

Setup config repo:
  $ cd "$TESTTMP"
  $ setup_configerator_configs
  $ INFINITEPUSH_ALLOW_WRITES=true \
  >   INFINITEPUSH_NAMESPACE_REGEX='^scratch/.+$' \
  >   create_large_small_repo
  Adding synced mapping entry
  $ cd "$TESTTMP/mononoke-config"
  $ enable_pushredirect 1

start SCS server and mononoke
  $ start_and_wait_for_scs_server
  $ start_large_small_repo
  Starting Mononoke server
  $ init_local_large_small_clones

setup client repos
  $ cd "$TESTTMP/small-hg-client"
  $ enable pushrebase commitcloud
  $ setconfig infinitepush.server=false infinitepush.branchpattern="re:scratch/.+"
  $ setconfig remotenames.selectivepulldefault=master_bookmark,branch1,branch2
  $ cd "$TESTTMP/large-hg-client"
  $ enable pushrebase commitcloud
  $ setconfig infinitepush.server=false infinitepush.branchpattern="re:scratch/.+"
  $ setconfig remotenames.selectivepulldefault=master_bookmark,branch1,branch2

make some simple requests that we can use to check scuba logging

List repos - there should be two of them
  $ scsc repos
  large-mon
  small-mon

  $ scsc xrepo-lookup --source-repo small-mon --target-repo large-mon --bonsai-id $SMALL_MASTER_BONSAI
  bfcfb674663c5438027bcde4a7ae5024c838f76a
  $ scsc xrepo-lookup --source-repo large-mon --target-repo small-mon --bonsai-id $LARGE_MASTER_BONSAI
  11f848659bfcf77abd04f947883badd8efa88d26

More complex case: multiple remappings of a source commit, need to use hints
-- create two independent branches, which do not have small repo files
  $ cd "$TESTTMP/large-hg-client"
  $ hg up -q master_bookmark
  $ echo bla > large_only_file_1
  $ hg ci -Aqm "large-only commit #1"
  $ hg push --quiet -r . --to branch1 --create
  $ hg up -q master_bookmark
  $ echo bla > large_only_file_2
  $ hg ci -Aqm "large-only commit #2"
  $ hg push -r . -q --to branch2 --create
-- as they don't have small repo files the exact mode will yield no result for those commits
  $ scsc xrepo-lookup --source-repo large-mon --target-repo small-mon -B branch1
  11f848659bfcf77abd04f947883badd8efa88d26
  $ scsc xrepo-lookup --source-repo large-mon --target-repo small-mon -B branch1 --exact
  * error: bookmark 'branch1' does not exist or does not have an equivalent commit in the target repo (glob)
  
  [1]

-- on top of these, create two large repo commits that remap into the same small repo one
  $ hg up -q branch1
  $ echo bla > smallrepofolder/file
  $ hg ci -Aqm "multiple large repo commits mapped to a single small repo commit"
  $ hg push --quiet -r . --to branch1
  $ scsc xrepo-lookup --source-repo large-mon --target-repo small-mon --hg-commit-id $(hg log -T "{node}" -r branch1)
  08cd130a185ce9e162d6d98e5bc1724279c73368
  $ hg up -q branch2
  $ echo bla > smallrepofolder/file
  $ hg ci -Aqm "multiple large repo commits mapped to a single small repo commit"
  $ hg push --quiet -r . --to branch2
  $ scsc xrepo-lookup --source-repo large-mon --target-repo small-mon --hg-commit-id $(hg log -T "{node}" -r branch2)
  08cd130a185ce9e162d6d98e5bc1724279c73368

  $ function pre_xrepo_lookup_commit() {
  >  cd "$TESTTMP/small-hg-client"
  >  hg up -q 08cd130a185ce9e162d6d98e5bc1724279c73368
  >  echo $1 > file
  >  shift
  >  hg ci -Aqm "commit which needs candidate selection hints to sync to a large repo"
  >  hg cloud upload -r . -q
  > }

-- syncing small repo commits with ambiguous parent mapping fails
  $ pre_xrepo_lookup_commit blabla
-- fails to sync without a hint
  $ scsc xrepo-lookup --source-repo small-mon --target-repo large-mon --hg-commit-id $(hg whereami)
  *scsc: error: SourceControlService::commit_lookup_xrepo failed with internal_error(InternalError): InternalError: Too many rewritten candidates for *: *, * (may be more) (glob)
  [1]

-- syncs with different hints succeed
  $ pre_xrepo_lookup_commit blabla2 --force
  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-ancestor-of-bookmark=branch1 \
  > --no-ondemand-sync \
  > --json
  {"exists":false,"ids":{}}

  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-ancestor-of-bookmark=branch1
  58aee7eea5b2087a8f8a7b1ac7b647d9e7e1f7d1

  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-ancestor-of-bookmark=branch1 \
  > --no-ondemand-sync
  58aee7eea5b2087a8f8a7b1ac7b647d9e7e1f7d1

  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-ancestor-of-bookmark=branch1
  58aee7eea5b2087a8f8a7b1ac7b647d9e7e1f7d1

  $ pre_xrepo_lookup_commit blabla3
  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-ancestor-of-commit=$(hg log -T "{node}" -r branch1 --cwd "$TESTTMP/large-hg-client")
  d8947d7cade14add70ea26ea3bc780bfa42cf474

  $ pre_xrepo_lookup_commit blabla4
  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-descendant-of-bookmark=branch1
  95ab695a67ed4c9a0af22cb5cbd2a687ace4a472

  $ pre_xrepo_lookup_commit blabla5
  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-descendant-of-commit=$(hg log -T "{node}" -r branch1 --cwd "$TESTTMP/large-hg-client")
  aba865fde747f93538aca932edf40bd66b73de56

  $ pre_xrepo_lookup_commit blabla6
  $ scsc xrepo-lookup \
  > --source-repo=small-mon \
  > --target-repo=large-mon \
  > --hg-commit-id=$(hg whereami) \
  > --hint-exact-commit=$(hg log -T "{node}" -r branch1 --cwd "$TESTTMP/large-hg-client")
  eaf51a1307fb0fdf82d0251562f43e3b9d84689b

-- test that scs won't allow us to create or move a bookmark in the small repo
  $ export SCSC_WRITES_ENABLED=true
  $ scsc list-bookmarks -R small-mon
  master_bookmark                          11f848659bfcf77abd04f947883badd8efa88d26

  $ scsc create-bookmark -R small-mon -n small_bookmark --hg-commit-id=$(hg whereami)

  $ scsc move-bookmark -R small-mon -n master_bookmark --hg-commit-id=$(hg whereami)
  *scsc: error: SourceControlService::repo_move_bookmark failed with request_error(RequestError): RequestError: Cannot move shared bookmark 'master_bookmark' from small repo (glob)
  [1]

-- The new bookmark might not show up due to WBC being slow to catch up. But if it does show,
-- it will have the correct value.
  $ scsc list-bookmarks -R small-mon
  master_bookmark                          11f848659bfcf77abd04f947883badd8efa88d26
  small_bookmark                           2da288ea40ed383528841d91b243225ddb608d95 (?)
