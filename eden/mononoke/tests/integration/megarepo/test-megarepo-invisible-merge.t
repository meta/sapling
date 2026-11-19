# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.
#require slow

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-push-redirector.sh"

Setup repositories
  $ REPOTYPE="blob_files"
  $ MEG_REPOID=0
  $ FBS_REPOID=1
  $ OVR_REPOID=2

  $ REPOID=$MEG_REPOID REPONAME=meg-mon setup_common_config $REPOTYPE
  $ REPOID=$FBS_REPOID REPONAME=fbs-mon setup_common_config $REPOTYPE
  $ REPOID=$OVR_REPOID REPONAME=ovr-mon setup_common_config $REPOTYPE

  $ setup_commitsyncmap
  $ setup_configerator_configs

-- initial push-redirection setup redirects ovrsource into megarepo,
-- which is the large repo at this point
-- disable sql cache since we will be changing pushredirect settings a couple of times
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:sql_disable_auto_cache": true
  >   }
  > }
  > EOF
  $ enable_pushredirect 2

  $ cat >> "$HGRCPATH" <<EOF
  > [ui]
  > ssh="$DUMMYSSH"
  > [extensions]
  > amend=
  > pushrebase=
  > EOF

  $ function createfile { mkdir -p "$(dirname  $1)" && echo "$1" > "$1" && hg add -q "$1"; }
  $ function createfile_with_content { mkdir -p "$(dirname  $1)" && echo "$2" > "$1" && hg add -q "$1"; }

  $ cd $TESTTMP

-- init hg fbsource server repo
  $ cd $TESTTMP
  $ hginit_treemanifest fbs-mon
  $ cd fbs-mon
-- create an initial commit using testtool drawdag
  $ testtool_drawdag -R fbs-mon --no-default-files <<'EOF'
  > A
  > # modify: A "fbcode/fbcodefile_fbsource" "fbcode/fbcodefile_fbsource\n"
  > # modify: A "arvr/arvrfile_fbsource" "arvr/arvrfile_fbsource\n"
  > # modify: A "otherfile_fbsource" "otherfile_fbsource\n"
  > # bookmark: A master_bookmark
  > EOF
  A=b0eeea20bb5a84cf2c2fb6befdae9507c67d6d7837aef2f275ca315c1018fdf8

-- init hg ovrsource server repo
  $ cd $TESTTMP
  $ hginit_treemanifest ovr-mon
  $ cd ovr-mon
-- create an initial commit using testtool drawdag
  $ testtool_drawdag -R ovr-mon --no-default-files <<'EOF'
  > A
  > # modify: A "fbcode/fbcodefile_ovrsource" "fbcode/fbcodefile_ovrsource\n"
  > # modify: A "arvr/arvrfile_ovrsource" "arvr/arvrfile_ovrsource\n"
  > # modify: A "otherfile_ovrsource" "otherfile_ovrsource\n"
  > # modify: A "Research/researchfile_ovrsource" "Research/researchfile_ovrsource\n"
  > # bookmark: A master_bookmark
  > EOF
  A=813af7d6ec0fe85fc2982a7b8127df45aa0240fc38a3f1f83d3608a26aaa44ab

-- init hg megarepo server repo
  $ cd $TESTTMP
  $ hginit_treemanifest meg-mon
  $ cd meg-mon
-- create an initial commit using testtool drawdag
  $ testtool_drawdag -R meg-mon --no-default-files <<'EOF'
  > A
  > # modify: A "fbcode/fbcodefile_fbsource" "fbcode/fbcodefile_fbsource\n"
  > # modify: A ".fbsource-rest/arvr/arvrfile_fbsource" "arvr/arvrfile_fbsource\n"
  > # modify: A "otherfile_fbsource" "otherfile_fbsource\n"
  > # modify: A ".ovrsource-rest/fbcode/fbcodefile_ovrsource" "fbcode/fbcodefile_ovrsource\n"
  > # modify: A "arvr/arvrfile_ovrsource" "arvr/arvrfile_ovrsource\n"
  > # modify: A "arvr-legacy/otherfile_ovrsource" "otherfile_ovrsource\n"
  > # modify: A "arvr-legacy/Research/researchfile_ovrsource" "Research/researchfile_ovrsource\n"
  > # bookmark: A master_bookmark
  > EOF
  A=141871105196d9fe6d0b0b6ee508d07983b6a543ac5f751e2f7b5dfd6fcd5fba

Import and start mononoke
  $ cd "$TESTTMP"
  $ REPOID=$MEG_REPOID mononoke
  $ wait_for_mononoke

-- setup hg client repos
  $ cd "$TESTTMP"
  $ hg clone -q mono:fbs-mon fbs-hg-cnt --noupdate
  $ hg clone -q mono:ovr-mon ovr-hg-cnt --noupdate
  $ hg clone -q mono:meg-mon meg-hg-cnt --noupdate

Setup commit sync mapping
-- get some bonsai hashes to avoid magic strings later
  $ FBSOURCE_MASTER_BONSAI=$(mononoke_admin bookmarks --repo-id $FBS_REPOID get master_bookmark)
  $ OVRSOURCE_MASTER_BONSAI=$(mononoke_admin bookmarks --repo-id $OVR_REPOID get master_bookmark)
  $ MEGAREPO_MERGE_BONSAI=$(mononoke_admin bookmarks --repo-id $MEG_REPOID get master_bookmark)

-- insert sync mapping entry
  $ add_synced_commit_mapping_entry $FBS_REPOID $FBSOURCE_MASTER_BONSAI $MEG_REPOID $MEGAREPO_MERGE_BONSAI TEST_VERSION_NAME
  $ add_synced_commit_mapping_entry $OVR_REPOID $OVRSOURCE_MASTER_BONSAI $MEG_REPOID $MEGAREPO_MERGE_BONSAI TEST_VERSION_NAME
-- tell backsyncer that we're all caught up in ovrsource
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "INSERT INTO mutable_counters (repo_id, name, value) SELECT $OVR_REPOID, 'backsync_from_$MEG_REPOID', COALESCE(MAX(id), 0) FROM bookmarks_update_log WHERE repo_id = $MEG_REPOID";
  $ qualify_backsync_cursor_at_fixture_checkpoint "$MEG_REPOID" "$OVR_REPOID" master_bookmark master_bookmark "$MEGAREPO_MERGE_BONSAI" "$OVRSOURCE_MASTER_BONSAI"


Perform ovrsource pushrebase, make sure it is push-redirected into Megarepo
  $ cd "$TESTTMP/ovr-hg-cnt"
  $ hg up -q master_bookmark
  $ echo 1 > pushredirected_1 && hg addremove -q && hg ci -q -m pushredirected_1
  $ hg push -r . --to master_bookmark
  pushing rev f3217265d27a to destination mono:ovr-mon bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark
-- pushredirected_1 was correctly pushed to master_bookmark in ovrsource
  $ log -r master_bookmark
  @  pushredirected_1 [public;rev=1;f3217265d27a] remote/master_bookmark
  │
  ~
-- pushredirected_1 is also present in megarepo
  $ cd "$TESTTMP"/meg-hg-cnt
  $ hg pull -q
  $ log -r master_bookmark
  o  pushredirected_1 [public;rev=1;a386326a200d] remote/master_bookmark
  │
  ~
-- ensure that ovrsource root path ends up in megarepo's arvr-legacy
  $ hg up master_bookmark -q
  $ ls arvr-legacy
  Research
  otherfile_ovrsource
  pushredirected_1
- compare the working copies
  $ REPOIDLARGE=$MEG_REPOID REPOIDSMALL=$OVR_REPOID verify_wc $(hg log -r master_bookmark -T '{node}')

  $ cd "$TESTTMP/ovr-hg-cnt"
  $ hg up -q master_bookmark
  $ echo 2 > pushredirected_2 && hg addremove -q && hg ci -q -m pushredirected_2
  $ hg push -r . --to master_bookmark
  pushing rev 43653dc8751c to destination mono:ovr-mon bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark
-- pushredirected_2 was correctly pushed to master_bookmark
  $ log -r master_bookmark
  @  pushredirected_2 [public;rev=2;43653dc8751c] remote/master_bookmark
  │
  ~
-- pushredirected_2 is also present in the megarepo
  $ cd "$TESTTMP"/meg-hg-cnt
  $ hg pull -q
  $ log -r master_bookmark
  o  pushredirected_2 [public;rev=2;998920b302f8] remote/master_bookmark
  │
  ~
-- let's see what's where in megarepo
  $ hg up master_bookmark -q
  $ ls arvr-legacy
  Research
  otherfile_ovrsource
  pushredirected_1
  pushredirected_2
- compare the working copies
  $ REPOIDLARGE=$MEG_REPOID REPOIDSMALL=$OVR_REPOID verify_wc $(hg log -r master_bookmark -T '{node}')


Set current version of CommitSyncConfig to have fbsource as large repo,
but disable push-redirection until invisible merge is done
-- stop mononoke before changing config with large repo change
  $ killandwait $MONONOKE_PID

Add a new config version to "all" configs, this new version has fbsource as large repo.
  $ cp "$TEST_FIXTURES/commitsync/all_with_flipped_config.json" "$COMMIT_SYNC_CONF/all"

-- This is an expected state of our configs at the last restart before
-- the invisible merge
  $ cp "$TEST_FIXTURES/commitsync/flipped_config.json" "$COMMIT_SYNC_CONF/current"
  $ enable_pushredirect 2 false false
  $ cp "$TEST_FIXTURES/commitsync/flipped_config.toml" "$TESTTMP/mononoke-config/common/commitsyncmap.toml"
-- start mononoke
  $ mononoke
  $ wait_for_mononoke


Prepare for the invisible merge
1. Create an independent ovrsource DAG in fbsource
  $ cd "$TESTTMP/ovr-hg-cnt"
  $ hg push -q \
  >     --config experimental.narrow-heads=true \
  >     --config pull.httpbookmarks=false \
  >     --config extensions.pushrebase=! \
  >     --to ovrsource/pre_move_master \
  >     --create --force -r . \
  >     mono:fbs-mon
  warning: repository is unrelated
1.5. Mark independent ovrsource DAG in fbsource as preserved
  $ cd "$TESTTMP"
  $ hg log -T "{node}\n" --cwd "ovr-hg-cnt" \
  > | xargs -I {} sqlite3 monsql/sqlite_dbs "SELECT HEX(bcs_id) FROM bonsai_hg_mapping WHERE hg_cs_id = X'{}'" \
  > | sort \
  > | uniq \
  > | xargs -I {} sqlite3 monsql/sqlite_dbs "INSERT INTO synced_commit_mapping (small_repo_id, small_bcs_id, large_repo_id, large_bcs_id) VALUES ($OVR_REPOID, X'{}', $FBS_REPOID, X'{}')"

2. Move files on top of the intermediate DAG
  $ cd "$TESTTMP/fbs-hg-cnt"
  $ hg pull -q -B ovrsource/pre_move_master
  $ hg up -q ovrsource/pre_move_master
  $ mkdir arvr-legacy .ovrsource-rest
  $ hg mv fbcode .ovrsource-rest/
  moving fbcode/fbcodefile_ovrsource to .ovrsource-rest/fbcode/fbcodefile_ovrsource
  $ hg mv arvr .ovrsource-rest/arvr
  moving arvr/arvrfile_ovrsource to .ovrsource-rest/arvr/arvrfile_ovrsource
  $ hg mv otherfile_ovrsource pushredirected_1 pushredirected_2 Research arvr-legacy/
  moving Research/researchfile_ovrsource to arvr-legacy/Research/researchfile_ovrsource
  $ hg ci -m "move ovrsource files into place"
  $ hg -q push --to ovrsource/moved_master --create
3. Implement a gradual merge policy
  $ COMMIT_DATE="1985-09-04T00:00:00.00Z"
  $ cd "$TESTTMP"
  $ PREDELETES=($(mononoke_admin megarepo pre-merge-delete --repo-id $FBS_REPOID  \
  > --bookmark ovrsource/moved_master \
  >  -a author -m "merge preparation" \
  >  --even-chunk-size 2 \
  > --commit-date-rfc3339 "$COMMIT_DATE" 2>/dev/null))
  $ echo "${PREDELETES[0]}"
  * (glob)
  $ echo "${PREDELETES[1]}"
  * (glob)
  $ MOVED_MASTER=$(mononoke_admin bookmarks --repo-id $FBS_REPOID get ovrsource/moved_master)
  $ echo "$MOVED_MASTER"
  * (glob)
-- a list of commits we want to merge also includes the pre-delete commit
  $ TOMERGES=("${PREDELETES[@]}" "$MOVED_MASTER")
-- calculate to-merge working copy sizes, they should be gradually increasing
  $ cd "$TESTTMP/fbs-hg-cnt"
  $ for TOMERGE in "${TOMERGES[@]}"; do
  >  HGHASH=$(mononoke_admin convert --repo-id=$FBS_REPOID --from bonsai --to hg --derive $TOMERGE)
  >  hg up -q $HGHASH
  >  FILECOUNT=$(find . -path ./.hg -prune -o -type f -print | wc -l)
  >  echo "$HGHASH: $FILECOUNT files"
  > done
  e1fffeb909369766a664bfdcfeba3684b1350921: 2 files
  29d5f54eaa58592f0de6e66ff95480a1f8aeb64c: 4 files
  278538d7ca136ce6ba45516edbce02469e5bd356: 6 files

Do the invisible merge by gradually merging TOMERGES into master
  $ cd "$TESTTMP/fbs-hg-cnt"
  $ hg up -q master_bookmark
  $ MASTER_BEFORE_MERGES=$(hg log -r . -T "{node}")
  $ for TOMERGE in "${TOMERGES[@]}"; do
  >  CURRENT=$(hg log -r . -T "{node}")
  >  echo "Current: $CURRENT"
  >  echo "To merge: $TOMERGE"
  >  MERGE=$(mononoke_admin megarepo bonsai-merge --repo-id=$FBS_REPOID -i $CURRENT -i $TOMERGE -a author -m "merge execution" --commit-date-rfc3339 "$COMMIT_DATE")
  >  HGMERGE=$(mononoke_admin convert --repo-id=$FBS_REPOID --from bonsai --to hg --derive $MERGE)
  >  echo "Merged as (bonsai): $MERGE"
  >  echo "Merged as (hg): $HGMERGE"
  >  hg up -q $HGMERGE
  >  FILECOUNT_1=$([ -d ./.ovrsource-rest ] && find ./.ovrsource-rest -type f | wc -l)
  >  FILECOUNT_2=$([ -d ./arvr-legacy ] && find ./arvr-legacy -type f | wc -l)
  >  FILECOUNT=$(($FILECOUNT_1 + $FILECOUNT_2))
  >  echo "file count is: $FILECOUNT"
  >  mononoke_admin bookmarks --repo-id=$FBS_REPOID set master_bookmark $HGMERGE
  >  flush_mononoke_bookmarks
  >  echo "intermediate" >> fbcode/fbcodefile_fbsource
  >  hg debugmakepublic -r .
  >  hg ci -qm "intermediate commit between gradual merge commits"
  >  hg push -q --to master_bookmark
  > done
  Current: a77a3f22071a3289ac6f928b4092093e71aae061
  To merge: cebabad19ac8be96b888095b917bfe1ac2e002d66a75025e7f639f93ba436fa4
  Merged as (bonsai): 2f71e45f331a85a46ca2b30db10d3701c68fe9bbd0ed1145eed7c2c2a73138d0
  Merged as (hg): 54ed8c743812888172298eb744a6200702aa3c2e
  file count is: 2
  Updating publishing bookmark master_bookmark from b0eeea20bb5a84cf2c2fb6befdae9507c67d6d7837aef2f275ca315c1018fdf8 to 2f71e45f331a85a46ca2b30db10d3701c68fe9bbd0ed1145eed7c2c2a73138d0
  Current: da6451df3ff79a6397d51b5551f713ea1209da23
  To merge: 9823b0918638a386a2ddad37b60dfbf844650fb09fc2d9ac66dc44b6f5553ae4
  Merged as (bonsai): 1939f09a3a48a8264c3b3f089290343a066a75dadd0a9b4f958e026827a3a479
  Merged as (hg): c1c74770419c3d7213c9f265d6b68a814a5c4233
  file count is: 4
  Updating publishing bookmark master_bookmark from 832d5bbe85038b1a9cb9f5f92a05a1882da15af0baff1d02d6997f1e034e42c1 to 1939f09a3a48a8264c3b3f089290343a066a75dadd0a9b4f958e026827a3a479
  Current: 4e214ea0a9f2c7b76476bd18305796335038756c
  To merge: 005f16a1a75b12fa40bf342d00fde249882367f0c494499a606b4ebf44beae48
  Merged as (bonsai): 2991ba0563bbbdb9759f86c34ac74fa5cf8d641fcd80cc64cdcdb9521ec51b90
  Merged as (hg): f6470d2d3c4b94c7fa12c11eac3962a5de6a50a0
  file count is: 6
  Updating publishing bookmark master_bookmark from 5159a8eaf10456f6bc05154288620bef9040eccba0ebe7d5f1f06d75c72cb821 to 2991ba0563bbbdb9759f86c34ac74fa5cf8d641fcd80cc64cdcdb9521ec51b90
  $ hg pull -q
  $ hg log -r "$MASTER_BEFORE_MERGES::master_bookmark" -T "{phase} {desc|firstline}\n"
  public A
  public merge execution
  public intermediate commit between gradual merge commits
  public merge execution
  public intermediate commit between gradual merge commits
  public merge execution
  public intermediate commit between gradual merge commits


Create special marker commits in both repos, which can be just marked as rewritten into each other
  $ cd "$TESTTMP/ovr-hg-cnt"
  $ hg ci -qm "pre push-redirection marker" --config ui.allowemptycommit=True
  $ hg push -r . --to master_bookmark
  pushing rev 07ff3584268a to destination mono:ovr-mon bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark
  $ cd "$TESTTMP/fbs-hg-cnt"
  $ hg ci -qm "pre push-redirection marker" --config ui.allowemptycommit=True
  $ hg push -r . --to master_bookmark
  pushing rev 5be64eb21882 to destination mono:fbs-mon bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark

Set mutable counter for the backsyncer (we've synced everything up until now)
  $ LATEST_LOG_ENTRY_ID=$(sqlite3 $TESTTMP/monsql/sqlite_dbs "SELECT MAX(id) FROM bookmarks_update_log WHERE repo_id = $FBS_REPOID")
  $ sqlite3 $TESTTMP/monsql/sqlite_dbs "INSERT INTO mutable_counters (repo_id, name, value) VALUES ($OVR_REPOID, 'backsync_from_$FBS_REPOID', $LATEST_LOG_ENTRY_ID)"

Set working copy equivalence between ovrsource master and fbsource master
  $ FBSOURCE_MASTER_BONSAI=$(mononoke_admin bookmarks --repo-id $FBS_REPOID get master_bookmark)
  $ OVRSOURCE_MASTER_BONSAI=$(mononoke_admin bookmarks --repo-id $OVR_REPOID get master_bookmark)
  $ sqlite3 $TESTTMP/monsql/sqlite_dbs \
  > "INSERT INTO synced_working_copy_equivalence \
  >    (small_repo_id, small_bcs_id, large_repo_id, large_bcs_id, sync_map_version_name) \
  >  VALUES \
  >    ($OVR_REPOID, X'$OVRSOURCE_MASTER_BONSAI', $FBS_REPOID, X'$FBSOURCE_MASTER_BONSAI', 'TEST_VERSION_NAME_FLIPPED')"
  $ qualify_backsync_cursor_at_fixture_checkpoint "$FBS_REPOID" "$OVR_REPOID" master_bookmark master_bookmark "$FBSOURCE_MASTER_BONSAI" "$OVRSOURCE_MASTER_BONSAI"

Set current version of CommitSyncConfig to be push-redirecting ovrsource
into fbsource
  $ enable_pushredirect 2
  $ force_update_configerator

Perform ovrsource pushrebase, make sure it is push-redirected into Fbsource
  $ cd "$TESTTMP/ovr-hg-cnt"
  $ hg up -q master_bookmark
  $ echo 1 > pushredirected_3 && hg addremove -q && hg ci -q -m pushredirected_3
  $ hg push -r . --to master_bookmark
  pushing rev 007d06273bc3 to destination mono:ovr-mon bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark
-- pushredirected_3 was correctly pushed to master_bookmark in ovrsource
  $ log -r master_bookmark
  @  pushredirected_3 [public;rev=4;007d06273bc3] remote/master_bookmark
  │
  ~
-- make the bookmark change visible to other repos. the cache invalidates
-- itself on push but not across repos.
  $ flush_mononoke_bookmarks
-- pushredirected_3 is also present in megarepo
  $ cd "$TESTTMP"/fbs-hg-cnt
  $ hg pull -q
  $ log -r master_bookmark
  o  pushredirected_3 [public;rev=14;f2be84ab7d21] remote/master_bookmark
  │
  ~
-- ensure that ovrsource root path ends up in megarepo's arvr-legacy
  $ hg up master_bookmark -q
  $ ls arvr-legacy
  Research
  otherfile_ovrsource
  pushredirected_1
  pushredirected_2
  pushredirected_3
- compare the working copies
  $ REPOIDLARGE=$FBS_REPOID REPOIDSMALL=$OVR_REPOID verify_wc $(hg log -r master_bookmark -T '{node}')
