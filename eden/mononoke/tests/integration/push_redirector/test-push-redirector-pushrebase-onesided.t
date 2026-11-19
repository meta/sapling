# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

setup configuration
  $ setconfig push.edenapi=true
  $ REPOTYPE="blob_files"
  $ REPOID=0 REPONAME=large setup_common_config $REPOTYPE
  $ REPOID=1 REPONAME=small-1 setup_common_config $REPOTYPE
  $ REPOID=2 REPONAME=small-2 setup_common_config $REPOTYPE
  $ cat >> "$TESTTMP/mononoke-config/common/commitsyncmap.toml" <<EOF
  > [megarepo_test]
  > large_repo_id = 0
  > common_pushrebase_bookmarks = ["master_bookmark"]
  > version_name = "TEST_VERSION_NAME_LIVE"
  >   [[megarepo_test.small_repos]]
  >   repoid = 1
  >   bookmark_prefix = "bookprefix1/"
  >   default_action = "prepend_prefix"
  >   default_prefix = "smallrepofolder1"
  >   direction = "large_to_small"
  >      [megarepo_test.small_repos.mapping]
  >      "special"="specialsmallrepofolder1"
  >   [[megarepo_test.small_repos]]
  >   repoid = 2
  >   bookmark_prefix = "bookprefix2/"
  >   default_action = "prepend_prefix"
  >   default_prefix = "smallrepofolder2"
  >   direction = "small_to_large"
  >      [megarepo_test.small_repos.mapping]
  >      "special"="specialsmallrepofolder2"
  > EOF

setup configerator configs
  $ setup_configerator_configs
  $ enable_pushredirect 1
  $ cat > "$COMMIT_SYNC_CONF/current" <<EOF
  > {
  >   "repos": {
  >     "megarepo_test": {
  >         "large_repo_id": 0,
  >         "common_pushrebase_bookmarks": [
  >           "master_bookmark"
  >         ],
  >         "small_repos": [
  >           {
  >             "repoid": 1,
  >             "default_action": "prepend_prefix",
  >             "default_prefix": "smallrepofolder1",
  >             "bookmark_prefix": "bookprefix1/",
  >             "mapping": {
  >               "special": "specialsmallrepofolder1"
  >             },
  >             "direction": "large_to_small"
  >           },
  >           {
  >             "repoid": 2,
  >             "default_action": "prepend_prefix",
  >             "default_prefix": "smallrepofolder2",
  >             "bookmark_prefix": "bookprefix2/",
  >             "mapping": {
  >               "special": "specialsmallrepofolder2"
  >             },
  >             "direction": "small_to_large"
  >           }
  >         ],
  >         "version_name": "TEST_VERSION_NAME_LIVE"
  >     }
  >   }
  > }
  > EOF
  $ cat > "$COMMIT_SYNC_CONF/all" <<EOF
  > {
  >   "repos": {
  >     "megarepo_test": {
  >       "versions": [
  >         {
  >           "large_repo_id": 0,
  >           "common_pushrebase_bookmarks": [
  >             "master_bookmark"
  >           ],
  >           "small_repos": [
  >             {
  >               "repoid": 1,
  >               "default_action": "prepend_prefix",
  >               "default_prefix": "smallrepofolder1",
  >               "bookmark_prefix": "bookprefix1/",
  >               "mapping": {
  >                 "special": "specialsmallrepofolder1"
  >               },
  >               "direction": "large_to_small"
  >             },
  >             {
  >               "repoid": 2,
  >               "default_action": "prepend_prefix",
  >               "default_prefix": "smallrepofolder2",
  >               "bookmark_prefix": "bookprefix2/",
  >               "mapping": {
  >                 "special": "specialsmallrepofolder2"
  >               },
  >               "direction": "small_to_large"
  >             }
  >           ],
  >           "version_name": "TEST_VERSION_NAME_LIVE"
  >         }
  >       ],
  >      "common": {
  >        "common_pushrebase_bookmarks": ["master_bookmark"],
  >        "large_repo_id": 0,
  >        "small_repos": {
  >          1: {
  >            "bookmark_prefix": "bookprefix1/"
  >          },
  >          2: {
  >            "bookmark_prefix": "bookprefix2/"
  >          }
  >        }
  >      }
  >     }
  >   }
  > }
  > EOF

  $ setconfig remotenames.selectivepulldefault=master_bookmark,bookprefix1/master_bookmark_non_fast_forward,bookprefix1/master_bookmark_2

Verification function
  $ function verify_wc() {
  >   local large_repo_commit
  >   large_repo_commit="$1"
  >   render_mononoke_config
  >   GLOG_minloglevel=5 "$MONONOKE_ADMIN" "${CACHE_ARGS[@]}" "${COMMON_ARGS[@]}" --log-level ERROR --config-tier "$MONONOKE_CONFIG_TIER" cross-repo --source-repo-id="$REPOIDLARGE" --target-repo-id="$REPOIDSMALL1" verify-working-copy $large_repo_commit
  > }

setup hg server repos
  $ cd "$TESTTMP"

  $ quiet testtool_drawdag -R small-1 --no-default-files <<EOF
  > S1_B
  > |
  > S1_A
  > # message: S1_A "pre-move commit 1"
  > # author: S1_A test
  > # modify: S1_A file.txt "1\n"
  > # message: S1_B "first post-move commit"
  > # author: S1_B test
  > # modify: S1_B filetoremove "1\n"
  > # bookmark: S1_B master_bookmark
  > EOF

  $ quiet testtool_drawdag -R small-2 --no-default-files <<EOF
  > S2_A
  > # message: S2_A "pre-move commit 2"
  > # author: S2_A test
  > # modify: S2_A file.txt "2\n"
  > # bookmark: S2_A master_bookmark
  > EOF

  $ quiet testtool_drawdag -R large --no-default-files <<EOF
  > L_D
  > |
  > L_C
  > |
  > L_B
  > |
  > L_A
  > # message: L_A "pre-move commit 1"
  > # author: L_A test
  > # modify: L_A file.txt "1\n"
  > # message: L_B "move commit"
  > # author: L_B test
  > # copy: L_B smallrepofolder1/file.txt "1\n" L_A file.txt
  > # delete: L_B file.txt
  > # message: L_C "move commit for repo 2"
  > # author: L_C test
  > # modify: L_C smallrepofolder1/file.txt "1\n"
  > # modify: L_C smallrepofolder2/file.txt "2\n"
  > # message: L_D "first post-move commit"
  > # author: L_D test
  > # modify: L_D smallrepofolder1/filetoremove "1\n"
  > # modify: L_D smallrepofolder2/file.txt "2\n"
  > # bookmark: L_D master_bookmark
  > EOF

  $ REPOIDLARGE=0
  $ REPOIDSMALL1=1
  $ REPOIDSMALL2=2
  $ LARGE_MASTER_BONSAI=$L_D
  $ SMALL1_MASTER_BONSAI=$S1_B
  $ SMALL2_MASTER_BONSAI=$S2_A

start mononoke server
  $ start_and_wait_for_mononoke_server
Make sure mapping is set up and we know what we don't have to sync initial entries
  $ add_synced_commit_mapping_entry $REPOIDSMALL1 $SMALL1_MASTER_BONSAI $REPOIDLARGE $LARGE_MASTER_BONSAI TEST_VERSION_NAME_LIVE
  $ add_synced_commit_mapping_entry $REPOIDSMALL2 $SMALL2_MASTER_BONSAI $REPOIDLARGE $LARGE_MASTER_BONSAI TEST_VERSION_NAME_LIVE
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "INSERT INTO mutable_counters (repo_id, name, value) SELECT $REPOIDSMALL1, 'backsync_from_$REPOIDLARGE', COALESCE(MAX(id), 0) FROM bookmarks_update_log WHERE repo_id = $REPOIDLARGE";
  $ qualify_backsync_cursor_at_fixture_checkpoint "$REPOIDLARGE" "$REPOIDSMALL1" master_bookmark master_bookmark "$LARGE_MASTER_BONSAI" "$SMALL1_MASTER_BONSAI"

setup hg client repos
  $ function init_client() {
  > cd "$TESTTMP"
  > hg clone -q mono:"$1" "$2" --noupdate
  > cd "$TESTTMP/$2"
  > cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > EOF
  > }

  $ init_client small-1 small-hg-client-1
  $ init_client small-2 small-hg-client-2
  $ cd "$TESTTMP"
  $ init_client large large-hg-client

Normal pushrebase with one commit
  $ cd "$TESTTMP/small-hg-client-1"
  $ hg up -q master_bookmark
  $ echo 2 > 2 && hg addremove -q && hg ci -q -m newcommit
  $ hg push -r . --to master_bookmark 2>&1 | grep "updated remote bookmark"
  updated remote bookmark master_bookmark to 6989db12d1e5
-- newcommit was correctly pushed to master_bookmark
  $ log -r master_bookmark
  @  newcommit [public;rev=2;6989db12d1e5] remote/master_bookmark
  │
  ~

-- newcommit is also present in the large repo (after a pull)
  $ cd "$TESTTMP"/large-hg-client
  $ log -r master_bookmark
  o  first post-move commit [public;rev=3;7b4785fb6152] remote/master_bookmark
  │
  ~
  $ hg pull -q
  $ log -r master_bookmark
  o  newcommit [public;rev=4;f47bdebb4c79] remote/master_bookmark
  │
  ~
- compare the working copies
  $ verify_wc $(hg log -r master_bookmark -T '{node}')

At the same time, the tailed repo gets new commits
  $ cd "$TESTTMP/small-hg-client-2"
  $ hg up -q master_bookmark
  $ createfile file2_1
  $ hg ci -qm "Post-merge commit 1"
  $ hg push --to master_bookmark -q
-- tailer puts this commit into a large repo
  $ mononoke_x_repo_sync $REPOIDSMALL2 $REPOIDLARGE once --target-bookmark master_bookmark -B master_bookmark 2>&1 | grep "synced as"
  * changeset * synced as * (glob)

Force pushrebase should fail, because it pushes to a shared bookmark
  $ cd "$TESTTMP/small-hg-client-1"
  $ hg up -q master_bookmark^
  $ echo 3 > 3 && hg add 3 && hg ci -q -m "non-forward move"
  $ hg push --to master_bookmark --force --pushvar NON_FAST_FORWARD=true >/dev/null
  pushing * (glob)
  abort: server error: invalid request: Cannot move shared bookmark 'master_bookmark' from small repo
  [255]

Non-shared bookmark should work
  $ hg push --to master_bookmark_non_fast_forward --force --create -q
-- it should also be present in a large repo
  $ cd "$TESTTMP/large-hg-client"
  $ hg pull -q
  $ log -r bookprefix1/master_bookmark_non_fast_forward
  o  non-forward move [public;rev=281474976710656;1ebb56d88b81] remote/bookprefix1/master_bookmark_non_fast_forward
  │
  ~

Bookmark-only pushrebase (Create a new bookmark, do not push commits)
  $ cd "$TESTTMP/small-hg-client-1"
  $ hg push -r master_bookmark^ --to master_bookmark_2 --create 2>&1 | grep creating
  creating remote bookmark master_bookmark_2
  $ hg book --all
  no bookmarks set
     remote/master_bookmark    6989db12d1e5
     remote/master_bookmark_2  680aaf36d7a2
     remote/master_bookmark_non_fast_forward 161addaa86c7
-- this is not a `common_pushrebase_bookmark`, so should be prefixed
  $ cd "$TESTTMP/large-hg-client"
  $ hg pull -q
  $ hg book --all
  no bookmarks set
     remote/bookprefix1/master_bookmark_2 7b4785fb6152
     remote/bookprefix1/master_bookmark_non_fast_forward 1ebb56d88b81
     remote/master_bookmark    1974f31a7d81
- compare the working copies
  $ verify_wc $(hg log -r bookprefix1/master_bookmark_2 -T '{node}')

Delete a bookmark
  $ cd "$TESTTMP/small-hg-client-1"
  $ quiet_grep deleting -- hg push --delete master_bookmark_2
  deleting remote bookmark master_bookmark_2
  $ hg book --all
  no bookmarks set
     remote/master_bookmark    6989db12d1e5
     remote/master_bookmark_non_fast_forward 161addaa86c7
  $ cd "$TESTTMP/large-hg-client"
  $ hg pull -q
  $ hg book --all
  no bookmarks set
     remote/bookprefix1/master_bookmark_non_fast_forward 1ebb56d88b81
     remote/master_bookmark    1974f31a7d81
