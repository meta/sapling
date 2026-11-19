# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.
#require slow

  $ export COMMIT_SCRIBE_CATEGORY=mononoke_commits
  $ export BOOKMARK_SCRIBE_CATEGORY=mononoke_bookmark
  $ export LARGE_REPO_ID=0
  $ export LARGE_REPO_NAME="large-mon"
  $ export SMALL_REPO_ID=1
  $ export SMALL_REPO_NAME="small-mon"
  $ export IMPORTED_REPO_ID=2
  $ export ANOTHER_REPO_ID=3
  $ export MASTER_BOOKMARK="master_bookmark"


  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

  $ cat >> $HGRCPATH <<EOF
  > [extensions]
  > rebase =
  > globalrevs =
  > EOF
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:cross_repo_skip_backsyncing_ordinary_empty_commits": true
  >   }
  > }
  > EOF

-- Init the imported repos
  $ IMPORTED_REPO_NAME="imported_repo"
  $ REPOID="$IMPORTED_REPO_ID" REPONAME="$IMPORTED_REPO_NAME" setup_common_config "blob_files"
  $ ANOTHER_REPO_NAME="another_repo"
  $ REPOID="$ANOTHER_REPO_ID" REPONAME="$ANOTHER_REPO_NAME" setup_common_config "blob_files"

-- Init large and small repos
  $ GLOBALREVS_PUBLISHING_BOOKMARK=$MASTER_BOOKMARK GLOBALREVS_SMALL_REPO_ID=$SMALL_REPO_ID \
  > REPOID=$LARGE_REPO_ID INFINITEPUSH_ALLOW_WRITES=true REPONAME=$LARGE_REPO_NAME \
  > setup_common_config blob_files
  $ DISALLOW_NON_PUSHREBASE=1 GLOBALREVS_PUBLISHING_BOOKMARK=$MASTER_BOOKMARK \
  > REPOID=$SMALL_REPO_ID REPONAME=$SMALL_REPO_NAME \
  > setup_common_config blob_files
  $ large_small_megarepo_config
  $ large_small_setup
  Adding synced mapping entry
  $ setup_configerator_configs
  $ enable_pushredirect 1 false true
  $ enable_pushredirect 2 false false
  $ enable_pushredirect 3 false false

  $ start_large_small_repo
  Starting Mononoke server
  $ init_local_large_small_clones

-- Start up the backsyncer in the background
  $ backsync_large_to_small_forever
-- Setup initial globalrevs
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ quiet testtool_drawdag -R $LARGE_REPO_NAME --no-default-files <<EOF
  > L_C-L_D
  > # extra: L_D  global_rev "1000157970"
  > # modify: L_D smallrepofolder/file.txt "22\n"
  > # bookmark: L_D $MASTER_BOOKMARK
  > # exists: L_C $LARGE_MASTER_BONSAI
  > EOF
  $ set_bonsai_globalrev_mapping "$LARGE_REPO_ID" "$L_D" 1000157970

Before config change
-- push to a large repo
  $ cd "$TESTTMP"/large-hg-client
  $ hg up -q $MASTER_BOOKMARK

  $ mkdir -p smallrepofolder
  $ echo bla > smallrepofolder/bla
  $ hg ci -Aqm "before merge"
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ hg push -r . --to $MASTER_BOOKMARK -q
  $ log_globalrev -r $MASTER_BOOKMARK
  o  before merge [public;globalrev=1000157971;a94d137602c0] remote/master_bookmark
  │
  ~

-- check the same commit in the small repo
  $ wait_for_bookmark_move_away_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK $PREV_BOOK_VALUE
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -q
  $ hg up -q $MASTER_BOOKMARK
  $ log_globalrev -r $MASTER_BOOKMARK
  @  before merge [public;globalrev=1000157971;61807722d4ec] remote/master_bookmark
  │
  ~
  $ hg log -r $MASTER_BOOKMARK -T "{files % '{file}\n'}"
  bla

-- config change
  $ update_commit_sync_map_for_new_repo_import

-- let LiveCommitSyncConfig pick up the changes
  $ force_update_configerator
-- populated imported repo that we're going to merge in
  $ testtool_drawdag -R "$IMPORTED_REPO_NAME" --no-default-files <<EOF
  > IA-IB-IC
  > # modify: IA "foo/a.txt" "creating foo directory"
  > # modify: IA "bar/b.txt" "creating bar directory"
  > # modify: IB "bar/c.txt" "random change"
  > # modify: IB "foo/d" "another random change"
  > # copy: IC "foo/b.txt" "copying file from bar into foo" IB "bar/b.txt"
  > # bookmark: IC heads/$MASTER_BOOKMARK
  > EOF
  IA=84c956fabb06e66011b9ad0c8f12a17995b86d66b949ebb08a320d91b6ab7646
  IB=e1238541007d381b788b0aaab2425ed3aad02e38afd80b4e85bb922deb452972
  IC=65f0b76c034d87adf7dac6f0b5a5442ab3f62edda21adb8e8ec57d1a99fb5905

  $ mononoke_x_repo_sync "$IMPORTED_REPO_ID"  "$LARGE_REPO_ID" initial-import -i "$IC" --version-name "imported_noop"
  [INFO] Starting session with id * (glob)
  [INFO] Starting up X Repo Sync from small repo imported_repo to large repo large-mon
  [INFO] Checking if 65f0b76c034d87adf7dac6f0b5a5442ab3f62edda21adb8e8ec57d1a99fb5905 is already synced 2->0
  [INFO] Syncing 65f0b76c034d87adf7dac6f0b5a5442ab3f62edda21adb8e8ec57d1a99fb5905 for initial import
  [INFO] Source repo: imported_repo / Target repo: large-mon
  [INFO] Found 3 unsynced ancestors
  [INFO] changeset 65f0b76c034d87adf7dac6f0b5a5442ab3f62edda21adb8e8ec57d1a99fb5905 synced as ecc8ec74d00988653ae64ebf206a9ed42898449125b91f59ecd1d8a0a93f4a97 in * (glob)
  [INFO] successful sync of head 65f0b76c034d87adf7dac6f0b5a5442ab3f62edda21adb8e8ec57d1a99fb5905
  [INFO] X Repo Sync execution finished from small repo imported_repo to large repo large-mon

  $ mononoke_admin fetch -R $LARGE_REPO_NAME  -i ecc8ec74d00988653ae64ebf206a9ed42898449125b91f59ecd1d8a0a93f4a97 --json | jq .parents
  [
    "fa5173cebb32a908f52fd6f01b442a76f013bda5b3d4bbcf3e29af0227bbb74f"
  ]

-- use gradual merge to merge in just one commit (usually this one does sequence of merges)
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ COMMIT_DATE="1985-09-04T00:00:00.00Z"
  $ quiet_grep merging -- mononoke_admin megarepo gradual-merge \
  > --repo-id $LARGE_REPO_ID -a test_user -m "gradual merge" \
  > --pre-deletion-commit -i fa5173cebb32a908f52fd6f01b442a76f013bda5b3d4bbcf3e29af0227bbb74f \
  > --last-deletion-commit -i ecc8ec74d00988653ae64ebf206a9ed42898449125b91f59ecd1d8a0a93f4a97 \
  > --target-bookmark $MASTER_BOOKMARK \
  > --limit 1 \
  > --commit-date-rfc3339 "$COMMIT_DATE"
  [INFO] merging 1 commits

-- check that merge has made into large repo
  $ cd "$TESTTMP"/large-hg-client
  $ hg -q pull
  $ hg up -q $MASTER_BOOKMARK
  $ log_globalrev -r $MASTER_BOOKMARK
  @    [MEGAREPO GRADUAL MERGE] gradual merge (0) [public;globalrev=1000157972;9af7a2bbf0f5] remote/master_bookmark
  ├─╮
  │ │
  ~ ~

-- push to a large repo on top of merge
  $ mkdir -p smallrepofolder
  $ echo baz > smallrepofolder/baz
  $ hg ci -Aqm "after merge"
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ hg push -r . --to $MASTER_BOOKMARK -q
  $ log_globalrev -r $MASTER_BOOKMARK
  o  after merge [public;globalrev=1000157973;1220098b4cde] remote/master_bookmark
  │
  ~

-- check the same commit in the small repo
  $ wait_for_bookmark_move_away_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK $PREV_BOOK_VALUE
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -q
  $ hg up -q $MASTER_BOOKMARK
  $ log_globalrev -r $MASTER_BOOKMARK^::$MASTER_BOOKMARK
  @  after merge [public;globalrev=1000157973;3381b75593e5] remote/master_bookmark
  │
  o  [MEGAREPO GRADUAL MERGE] gradual merge (0) [public;globalrev=1000157972;9351f7816915]
  │
  ~

  $ echo baz_from_small > baz
  $ hg ci -Aqm "after merge from small"
  $ hg push -r . --to $MASTER_BOOKMARK -q
  $ log_globalrev -r $MASTER_BOOKMARK^::$MASTER_BOOKMARK
  o  after merge from small [public;globalrev=1000157974;c17052372d27] remote/master_bookmark
  │
  o  after merge [public;globalrev=1000157973;3381b75593e5]
  │
  ~
  $ cd "$TESTTMP/large-hg-client"
  $ hg pull -q
  $ log_globalrev -r $MASTER_BOOKMARK^::$MASTER_BOOKMARK
  o  after merge from small [public;globalrev=1000157974;4d44ba9e1ca3] remote/master_bookmark
  │
  o  after merge [public;globalrev=1000157973;1220098b4cde]
  │
  ~


  $ testtool_drawdag -R "$IMPORTED_REPO_NAME" <<EOF
  > IC-ID
  > # exists: IC $IC
  > # bookmark: ID $MASTER_BOOKMARK
  > EOF
  IC=65f0b76c034d87adf7dac6f0b5a5442ab3f62edda21adb8e8ec57d1a99fb5905
  ID=a14dee507f7605083e9a99901971ac7c5558d8b28d7d01090bd2cff2432fa707

  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ mononoke_x_repo_sync "$IMPORTED_REPO_ID"  "$LARGE_REPO_ID" \
  > once --commit-id "$ID" --unsafe-change-version-to "new_version" --target-bookmark $MASTER_BOOKMARK
  [INFO] Starting session with id * (glob)
  [INFO] Starting up X Repo Sync from small repo imported_repo to large repo large-mon
  [INFO] Syncing 1 commits and all of their unsynced ancestors
  [INFO] Checking if a14dee507f7605083e9a99901971ac7c5558d8b28d7d01090bd2cff2432fa707 is already synced 2->0
  [WARN] Changing mapping version during pushrebase to new_version
  [INFO] 1 unsynced ancestors of a14dee507f7605083e9a99901971ac7c5558d8b28d7d01090bd2cff2432fa707
  [WARN] target bookmark is not wc-equivalent to synced commit, falling back to parent_version
  [WARN] UNSAFE: changing mapping version during pushrebase to new_version
  [INFO] syncing a14dee507f7605083e9a99901971ac7c5558d8b28d7d01090bd2cff2432fa707 via pushrebase for master_bookmark
  [INFO] changeset a14dee507f7605083e9a99901971ac7c5558d8b28d7d01090bd2cff2432fa707 synced as 402c52f0f2156a83bf5354aae35c3cae55e92b23da3ed61bc10ee7960e172c8e * (glob)
  [INFO] successful sync
  [INFO] X Repo Sync execution finished from small repo imported_repo to large repo large-mon

  $ wait_for_bookmark_move_away_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK $PREV_BOOK_VALUE
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -q
  $ hg update $MASTER_BOOKMARK
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ log_globalrev -r .^::.
  @  ID [public;globalrev=1000157975;8d707fde6f5e] remote/master_bookmark
  │
  o  after merge from small [public;globalrev=1000157974;c17052372d27]
  │
  ~
  $ echo baz_from_small2 > bar
  $ hg add bar
  $ hg ci -Aqm "after mapping change from small"
  $ hg push -r . --to $MASTER_BOOKMARK -q

  $ log_globalrev -r $MASTER_BOOKMARK^::$MASTER_BOOKMARK
  o  after mapping change from small [public;globalrev=1000157976;ecca553b5690] remote/master_bookmark
  │
  o  ID [public;globalrev=1000157975;8d707fde6f5e]
  │
  ~

  $ cd "$TESTTMP/large-hg-client"
  $ hg pull -q
  $ log_globalrev -r $MASTER_BOOKMARK^::$MASTER_BOOKMARK
  o  after mapping change from small [public;globalrev=1000157976;54bd67a132c8] remote/master_bookmark
  │
  o  ID [public;globalrev=1000157975;4f56877f458b]
  │
  ~

  $ testtool_drawdag -R "$IMPORTED_REPO_NAME" <<EOF
  > ID-IE
  > # exists: ID $ID
  > # bookmark: IE heads/$MASTER_BOOKMARK
  > EOF
  ID=a14dee507f7605083e9a99901971ac7c5558d8b28d7d01090bd2cff2432fa707
  IE=ee275b10c734fa09ff52acf808a3baafd24348114fa937e8f41958490b9b6857

  $ testtool_drawdag -R "$IMPORTED_REPO_NAME" <<EOF
  > IE-IF-IG
  > # exists: IE $IE
  > # bookmark: IG heads/$MASTER_BOOKMARK
  > EOF
  IE=ee275b10c734fa09ff52acf808a3baafd24348114fa937e8f41958490b9b6857
  IF=20d91840623a3e0e6f3bc3c46ce6755d5f4c9ce6cfb49dae7b9cc8d9d0acfae9
  IG=2daec24778b88c326d1ba0f830d43a2d24d471dc22c48c8307096d0f60c9477f
  $ quiet mononoke_admin mutable-counters --repo-id $LARGE_REPO_ID set xreposync_from_$IMPORTED_REPO_ID 2
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $LARGE_REPO_NAME $MASTER_BOOKMARK)
  $ quiet mononoke_x_repo_sync "$IMPORTED_REPO_ID"  "$LARGE_REPO_ID" tail --bookmark-regex "heads/$MASTER_BOOKMARK" --catch-up-once

  $ hg pull -q
  $ log_globalrev -r $MASTER_BOOKMARK^^^::$MASTER_BOOKMARK
  o  IG [public;globalrev=1000157979;0d969c3e772c] remote/master_bookmark
  │
  o  IF [public;globalrev=1000157978;a3fc14316d38]
  │
  o  IE [public;globalrev=1000157977;4d7edff71de1]
  │
  o  after mapping change from small [public;globalrev=1000157976;54bd67a132c8]
  │
  ~

-- The rest of this repo merges in another repo into large repo
-- this covers the scenario where we have to do a merge and mapping change in large
-- repo while there's forward sync going on.
  $ testtool_drawdag -R "$ANOTHER_REPO_NAME" --no-default-files <<EOF
  > AA-AB-AC
  > # modify: AA "foo/a.txt" "creating foo directory"
  > # modify: AA "bar/b.txt" "creating bar directory"
  > # modify: AB "bar/c.txt" "random change"
  > # modify: AB "foo/d" "another random change"
  > # copy: AC "foo/b.txt" "copying file from bar into foo" AB "bar/b.txt"
  > # bookmark: AC heads/$MASTER_BOOKMARK
  > EOF
  AA=be49ffcb679bb0485fd5cf5a05013e8554065d19796407d5ab97b556c951cd35
  AB=1bb09eba8700dd4438bf66006b0f70cce454b32adb9a168f1e8a2d04c19c0cd3
  AC=156943c35cda314d72b0177b06d5edf3c92dc9c9505d7b3171b9230f7c1768bb

  $ mononoke_x_repo_sync "$ANOTHER_REPO_ID"  "$LARGE_REPO_ID" initial-import -i "$AC" --version-name "another_noop"
  [INFO] Starting session with id * (glob)
  [INFO] Starting up X Repo Sync from small repo another_repo to large repo large-mon
  [INFO] Checking if 156943c35cda314d72b0177b06d5edf3c92dc9c9505d7b3171b9230f7c1768bb is already synced 3->0
  [INFO] Syncing 156943c35cda314d72b0177b06d5edf3c92dc9c9505d7b3171b9230f7c1768bb for initial import
  [INFO] Source repo: another_repo / Target repo: large-mon
  [INFO] Found 3 unsynced ancestors
  [INFO] changeset 156943c35cda314d72b0177b06d5edf3c92dc9c9505d7b3171b9230f7c1768bb synced as 0a9797a0fa6b3284b9d73ec43357f06a9b00d6fa402122d1bbfbeac16e3a2c39 * (glob)
  [INFO] successful sync of head 156943c35cda314d72b0177b06d5edf3c92dc9c9505d7b3171b9230f7c1768bb
  [INFO] X Repo Sync execution finished from small repo another_repo to large repo large-mon

  $ mononoke_admin fetch -R $LARGE_REPO_NAME  -i 0a9797a0fa6b3284b9d73ec43357f06a9b00d6fa402122d1bbfbeac16e3a2c39 --json | jq .parents
  [
    "7b877236dc63b9df21954f78b6c8ce8b69a844e786fe58a2932de04ac685075d"
  ]

-- use gradual merge to merge in just one commit (usually this one does sequence of merges)
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ COMMIT_DATE="2006-05-03T22:38:01.00Z"
  $ quiet_grep merging -- mononoke_admin megarepo gradual-merge \
  > --repo-id $LARGE_REPO_ID -a test_user \
  > -m "another merge" \
  > --pre-deletion-commit -i 7b877236dc63b9df21954f78b6c8ce8b69a844e786fe58a2932de04ac685075d \
  > --last-deletion-commit -i 0a9797a0fa6b3284b9d73ec43357f06a9b00d6fa402122d1bbfbeac16e3a2c39 \
  > --target-bookmark $MASTER_BOOKMARK \
  > --limit 1 \
  > --commit-date-rfc3339 "$COMMIT_DATE"
  [INFO] merging 1 commits

-- check that merge has made into large repo
  $ cd "$TESTTMP"/large-hg-client
  $ hg -q pull
  $ hg up -q $MASTER_BOOKMARK
  $ log_globalrev -r $MASTER_BOOKMARK
  @    [MEGAREPO GRADUAL MERGE] another merge (0) [public;globalrev=1000157980;c58d6329efff] remote/master_bookmark
  ├─╮
  │ │
  ~ ~

  $ testtool_drawdag -R "$ANOTHER_REPO_NAME" <<EOF
  > AC-AD
  > # exists: AC $AC
  > # bookmark: AD $MASTER_BOOKMARK
  > EOF
  AC=156943c35cda314d72b0177b06d5edf3c92dc9c9505d7b3171b9230f7c1768bb
  AD=1d0bbdb162c2887a5b93893d7a48fd852a304ab58be2245899bb795e80aa10e9

  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ mononoke_x_repo_sync "$ANOTHER_REPO_ID"  "$LARGE_REPO_ID" \
  > once --commit-id "$AD" --unsafe-change-version-to "another_version" --target-bookmark $MASTER_BOOKMARK
  [INFO] Starting session with id * (glob)
  [INFO] Starting up X Repo Sync from small repo another_repo to large repo large-mon
  [INFO] Syncing 1 commits and all of their unsynced ancestors
  [INFO] Checking if 1d0bbdb162c2887a5b93893d7a48fd852a304ab58be2245899bb795e80aa10e9 is already synced 3->0
  [WARN] Changing mapping version during pushrebase to another_version
  [INFO] 1 unsynced ancestors of 1d0bbdb162c2887a5b93893d7a48fd852a304ab58be2245899bb795e80aa10e9
  [WARN] target bookmark is not wc-equivalent to synced commit, falling back to parent_version
  [WARN] UNSAFE: changing mapping version during pushrebase to another_version
  [INFO] syncing 1d0bbdb162c2887a5b93893d7a48fd852a304ab58be2245899bb795e80aa10e9 via pushrebase for master_bookmark
  [INFO] changeset 1d0bbdb162c2887a5b93893d7a48fd852a304ab58be2245899bb795e80aa10e9 synced as 76b08a5702ff09571621ca88b107d886963d2c8265f508edc6e4d8f95777fd3e * (glob)
  [INFO] successful sync
  [INFO] X Repo Sync execution finished from small repo another_repo to large repo large-mon

  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi "$IMPORTED_REPO_NAME" heads/$MASTER_BOOKMARK)
  $ testtool_drawdag  --print-hg-hashes -R "$IMPORTED_REPO_NAME" <<EOF
  > IG-IH-II
  > # exists: IG $IG
  > # bookmark: II heads/$MASTER_BOOKMARK
  > EOF
  IG=8ab5c12e737d5da736082a535ed0fc66b234e957
  IH=390213545a07b8f0b3452f97e862443d56b58375
  II=6738aefcbd6e1d868fa73a489b55aab543fd0c53
  $ quiet mononoke_x_repo_sync "$IMPORTED_REPO_ID"  "$LARGE_REPO_ID" tail --bookmark-regex "heads/$MASTER_BOOKMARK" --catch-up-once

  $ FINAL_BOOK_VALUE=$(x_repo_lookup $IMPORTED_REPO_NAME $LARGE_REPO_NAME $II)

  $ mononoke_admin changelog -R $LARGE_REPO_NAME graph -i $FINAL_BOOK_VALUE -M
  o  message: II
  │
  o  message: IH
  │
  o  message: AD
  │
  o    message: [MEGAREPO GRADUAL MERGE] another merge (0)
  ├─╮
  o │  message: IG
  │ │
  o │  message: IF
  │ │
  o │  message: IE
  │ │
  o │  message: after mapping change from small
  │ │
  o │  message: ID
  │ │
  o │  message: after merge from small
  │ │
  o │  message: after merge
  │ │
  ~ │
    │
    o  message: AC
    │
    o  message: AB
    │
    o  message: AA

-- -------------- Introducing repo with git submodule expansion ----------------
NOTE: the output of many of these steps is not relevant to this integration test,
so they'll be dumped to files to keep this (already long) integration test shorter.

-- Define the large and small repo ids and names before calling any helpers
  $ export SUBMODULE_REPO_NAME="submodule_repo"
  $ export SUBMODULE_REPO_ID=11

  $ . "${TEST_FIXTURES}/cross_repo/library-git-submodules-repos-setup.sh"

-- Setting up mutable counter for live forward sync
-- NOTE: this might need to be updated/refactored when setting up test for backsyncing
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" \
  > "INSERT INTO mutable_counters (repo_id, name, value) \
  > VALUES ($LARGE_REPO_ID, 'xreposync_from_$SUBMODULE_REPO_ID', 1)";

  $ REPOID="$SUBMODULE_REPO_ID" REPONAME="$SUBMODULE_REPO_NAME" \
  > COMMIT_IDENTITY_SCHEME=3 setup_common_config "$REPOTYPE"

  $ REPOID="$REPO_C_ID" REPONAME="repo_c" \
  > COMMIT_IDENTITY_SCHEME=3  setup_common_config "$REPOTYPE"

  $ REPOID="$REPO_B_ID" REPONAME="repo_b" \
  > COMMIT_IDENTITY_SCHEME=3 setup_common_config "$REPOTYPE"

-- Update the commit sync config
  $ update_commit_sync_map_for_import_expanding_git_submodules
  $ force_update_configerator

  $ NOOP_CONFIG_VERSION_NAME="$SUBMODULE_NOOP_VERSION_NAME" \
  > CONFIG_VERSION_NAME="$AFTER_SUBMODULE_REPO_VERSION_NAME" \
  > MASTER_BOOKMARK="master_bookmark" quiet create_and_merge_submodule_repo

-- Set up live forward syncer, which should sync all commits in submodule repo's
-- heads/master bookmark to large repo's master bookmark via pushrebase
  $ touch $TESTTMP/xreposync.out
  $ mononoke_x_repo_sync_forever "$SUBMODULE_REPO_ID" "$LARGE_REPO_ID"


-- push to large repo on top of merge
  $ mkdir -p smallrepofolder
  $ echo "baz after merging submodule expansion" > smallrepofolder/baz
  $ hg ci -Aqm "after merging submodule expansion"
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ hg push -r . --to $MASTER_BOOKMARK -q
  $ log_globalrev -r $MASTER_BOOKMARK -l 10
  @  after merging submodule expansion [public;globalrev=;ffe35354096c] remote/master_bookmark
  │
  ~


-- Check if changes were backsynced properly
  $ wait_for_bookmark_move_away_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK $PREV_BOOK_VALUE
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -q
  $ log_globalrev -l 10
  @  after mapping change from small [draft;globalrev=;a4c70b6f0c57]
  │
  ~
  $
  o  after merge from small [draft;globalrev=;14c64221d993]
  │
  ~
  $
  o  after merging submodule expansion [public;globalrev=;5bc83a834e83] remote/master_bookmark
  │
  o  Added git repo C as submodule directly in A [public;globalrev=1000157988;69712c3f21b2]
  │
  o  [MEGAREPO GRADUAL MERGE] gradual merge (3) [public;globalrev=1000157987;29f4bdf73e54]
  │
  o  [MEGAREPO GRADUAL MERGE] gradual merge (2) [public;globalrev=1000157986;be3eeaa0b9a0]
  │
  o  [MEGAREPO GRADUAL MERGE] gradual merge (1) [public;globalrev=1000157985;f5cb09a7ec32]
  │
  o  [MEGAREPO GRADUAL MERGE] gradual merge (0) [public;globalrev=1000157984;63782775678a]
  │
  o  AD [public;globalrev=1000157981;cc919180ea26]
  │
  o  [MEGAREPO GRADUAL MERGE] another merge (0) [public;globalrev=1000157980;6db37bb0eca0]
  │
  ~

-- Make changes to the git repos and sync them to the submodule repo merged into
-- the large repo.
  $ make_changes_to_git_repos_a_b_c &> $TESTTMP/changes_to_git_repos.out


-- Go to large repo, make a change to small repo folder and push
  $ cd "$TESTTMP/large-hg-client"
  $ echo "file.txt after making changes to the submodule repo" > smallrepofolder/file.txt
  $ hg ci -Aqm "after live sync and changes to submodule repo"
  $ PREV_BOOK_VALUE=$(get_bookmark_value_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK)
  $ hg push -r . --to $MASTER_BOOKMARK -q
  $ log_globalrev -r $MASTER_BOOKMARK -l 30
  o  after live sync and changes to submodule repo [public;globalrev=1000157989;cf2c14f12677] remote/master_bookmark
  │
  ~


-- Check if changes were backsynced properly to small repo
  $ wait_for_bookmark_move_away_edenapi $SMALL_REPO_NAME $MASTER_BOOKMARK $PREV_BOOK_VALUE
  $ cd "$TESTTMP/small-hg-client"
  $ hg pull -q
  $ hg log -G -T "{desc} [{node|short}]\n" -l 30 --stat
  @  after mapping change from small [a4c70b6f0c57]
  │   bar |  1 +
  │   1 files changed, 1 insertions(+), 0 deletions(-)
  │
  │ o  after merge from small [14c64221d993]
  │ │   baz |  2 +-
  │ │   1 files changed, 1 insertions(+), 1 deletions(-)
  │ │
  │ │ o  after live sync and changes to submodule repo [7bea9eac2447]
  │ │ │   file.txt |  2 +-
  │ │ │   1 files changed, 1 insertions(+), 1 deletions(-)
  │ │ │
  │ │ o  after merging submodule expansion [5bc83a834e83]
  │ │ │   baz |  2 +-
  │ │ │   1 files changed, 1 insertions(+), 1 deletions(-)
  │ │ │
  │ │ o  Added git repo C as submodule directly in A [69712c3f21b2]
  │ │ │
  │ │ o  [MEGAREPO GRADUAL MERGE] gradual merge (3) [29f4bdf73e54]
  │ │ │
  │ │ o  [MEGAREPO GRADUAL MERGE] gradual merge (2) [be3eeaa0b9a0]
  │ │ │
  │ │ o  [MEGAREPO GRADUAL MERGE] gradual merge (1) [f5cb09a7ec32]
  │ │ │
  │ │ o  [MEGAREPO GRADUAL MERGE] gradual merge (0) [63782775678a]
  │ │ │
  │ │ o  AD [cc919180ea26]
  │ │ │
  │ │ o  [MEGAREPO GRADUAL MERGE] another merge (0) [6db37bb0eca0]
  │ │ │
  │ │ o  after mapping change from small [ecca553b5690]
  ├───╯   bar |  1 +
  │ │     1 files changed, 1 insertions(+), 0 deletions(-)
  │ │
  o │  ID [8d707fde6f5e]
  │ │
  o │  after merge from small [c17052372d27]
  ├─╯   baz |  2 +-
  │     1 files changed, 1 insertions(+), 1 deletions(-)
  │
  o  after merge [3381b75593e5]
  │   baz |  1 +
  │   1 files changed, 1 insertions(+), 0 deletions(-)
  │
  o  [MEGAREPO GRADUAL MERGE] gradual merge (0) [9351f7816915]
  │
  o  before merge [61807722d4ec]
  │   bla |  1 +
  │   1 files changed, 1 insertions(+), 0 deletions(-)
  │
  o  L_D [0f80c2748608]
  │   file.txt |  2 +-
  │   1 files changed, 1 insertions(+), 1 deletions(-)
  │
  o  first post-move commit [11f848659bfc]
  │   filetoremove |  1 +
  │   1 files changed, 1 insertions(+), 0 deletions(-)
  │
  o  pre-move commit [fc7ae591de0e]
      file.txt |  1 +
      1 files changed, 1 insertions(+), 0 deletions(-)
  
