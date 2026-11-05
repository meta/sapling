# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ setconfig push.edenapi=true
  $ setup_mononoke_config
  $ cd "$TESTTMP/mononoke-config"
  $ cat >> repos/repo/server.toml <<CONFIG
  > [[bookmarks]]
  > name="master_bookmark"
  > [[bookmarks]]
  > regex=".*"
  > CONFIG

  $ register_hook_limit_filesize_global_limit 10 'bypass_pushvar="ALLOW_LARGE_FILES=true"'

  $ setup_common_hg_configs
  $ setconfig remotenames.selectivepulldefault=master_bookmark,x
  $ cd $TESTTMP

  $ configure dummyssh
  $ enable amend

setup repo
  $ testtool_drawdag -R repo <<EOF
  > A X
  > # message: A "A"
  > # message: X "X"
  > # bookmark: A master_bookmark
  > # bookmark: X x
  > EOF
  A=* (glob)
  X=* (glob)

start mononoke
  $ start_and_wait_for_mononoke_server


clone
  $ hg clone -q mono:repo repo2 --noupdate
  $ cd repo2
  $ hg pull -q -B master_bookmark -B x
  $ enable pushrebase

make more commits
  $ A_HASH=$(hg log -r 'remote/master_bookmark' -T '{node}')
  $ X_HASH=$(hg log -r 'remote/x' -T '{node}')
  $ drawdag <<EOF
  > D F           # C/clarge = file_too_large
  > | |           # E/elarge = file_too_large
  > C E    Z      # Y/ylarge = file_too_large
  > |/     |
  > B      Y
  > |      |
  > |      $X_HASH
  > $A_HASH
  > EOF

fast-forward the bookmark
  $ hg up -q $B
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  pushrebasing stack (*, *] (1 commit) to remote bookmark master_bookmark (glob)
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master_bookmark to * (glob)

fast-forward the bookmark over a commit that fails the hook
  $ hg up -q $D
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 2 commits for upload
  edenapi: queue 3 files for upload
  edenapi: uploaded 3 files
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  abort: Server error: hooks failed:
    limit_filesize for *: File size limit is 10 bytes. You tried to push file clarge that is over the limit (14 bytes, 1.40x the limit). This limit is enforced for files matching the following regex: ".*". (glob)
  
  WHY THIS IS BLOCKED: Large files have ongoing infrastructure costs — they impact caching systems, Mononoke, biggrep indexing, and permanent backups used by 30,000+ engineers.
  
  ALTERNATIVES TO CONSIDER:
  - Manifold: Store large binaries in blob storage
  - Dotslash: Distribute large tools without checking them in
  - Buckify: Package binaries as Buck-managed dependencies
  - LFS: Use Git LFS for large files that must be versioned
  - Split files: Break large files into smaller pieces
  
  IF ALTERNATIVES DO NOT WORK:
  1. Add @allow-large-files to your commit message (using `sl amend -e`).
  2. Request bypass approval at https://fburl.com/support/sourcecontrol.
  
  See https://fburl.com/landing_big_diffs for more details.
  [255]

bypass the hook, the push will now work
  $ hg push -r . --to master_bookmark --pushvar ALLOW_LARGE_FILES=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master_bookmark to * (glob)

attempt a non-fast-forward move, it should fail
  $ hg up -q $F
  $ hg push -r . --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 2 commits for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  abort: Server error: hooks failed:
    limit_filesize for *: File size limit is 10 bytes. You tried to push file elarge that is over the limit (14 bytes, 1.40x the limit). This limit is enforced for files matching the following regex: ".*". (glob)
  
  WHY THIS IS BLOCKED: Large files have ongoing infrastructure costs — they impact caching systems, Mononoke, biggrep indexing, and permanent backups used by 30,000+ engineers.
  
  ALTERNATIVES TO CONSIDER:
  - Manifold: Store large binaries in blob storage
  - Dotslash: Distribute large tools without checking them in
  - Buckify: Package binaries as Buck-managed dependencies
  - LFS: Use Git LFS for large files that must be versioned
  - Split files: Break large files into smaller pieces
  
  IF ALTERNATIVES DO NOT WORK:
  1. Add @allow-large-files to your commit message (using `sl amend -e`).
  2. Request bypass approval at https://fburl.com/support/sourcecontrol.
  
  See https://fburl.com/landing_big_diffs for more details.
  [255]
specify the pushvar to allow the non-fast-forward move.
  $ hg push -r . --to master_bookmark --pushvar NON_FAST_FORWARD=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  abort: Server error: hooks failed:
    limit_filesize for *: File size limit is 10 bytes. You tried to push file elarge that is over the limit (14 bytes, 1.40x the limit). This limit is enforced for files matching the following regex: ".*". (glob)
  
  WHY THIS IS BLOCKED: Large files have ongoing infrastructure costs — they impact caching systems, Mononoke, biggrep indexing, and permanent backups used by 30,000+ engineers.
  
  ALTERNATIVES TO CONSIDER:
  - Manifold: Store large binaries in blob storage
  - Dotslash: Distribute large tools without checking them in
  - Buckify: Package binaries as Buck-managed dependencies
  - LFS: Use Git LFS for large files that must be versioned
  - Split files: Break large files into smaller pieces
  
  IF ALTERNATIVES DO NOT WORK:
  1. Add @allow-large-files to your commit message (using `sl amend -e`).
  2. Request bypass approval at https://fburl.com/support/sourcecontrol.
  
  See https://fburl.com/landing_big_diffs for more details.
  [255]

bypass the hook too, and it should work
  $ hg push -r . --to master_bookmark --pushvar NON_FAST_FORWARD=true --pushvar ALLOW_LARGE_FILES=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  pushrebasing stack (*, *] (2 commits) to remote bookmark master_bookmark (glob)
  3 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master_bookmark to * (glob)

Noop bookmark-only push doesn't need to bypass hooks to go through.
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select count(*) from bookmarks_update_log";
  5
The server side bookmark value can be stable due to data derivation, let's workaround it by reading from local
  $ hg push -r . --to master_bookmark --config push.use_local_bookmark_value=True
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  moving remote bookmark master_bookmark from * to * (glob)
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select count(*) from bookmarks_update_log";
  5

attempt to pushrebase onto a completely unrelated commit (no common ancestor), with an
ancestor that fails the hook. The client finds no merge base to rebase from, so it
aborts before the hook runs, with or without the hook bypass
  $ hg up -q $Z
  $ hg push -r . --to master_bookmark --pushvar NON_FAST_FORWARD=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  edenapi: queue 2 commits for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  abort: cannot pushrebase * onto remote bookmark master_bookmark: they have no common ancestor (glob)
  (rebase the stack onto master_bookmark first)
  [255]

  $ hg push -r . --to master_bookmark --pushvar NON_FAST_FORWARD=true --pushvar ALLOW_LARGE_FILES=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  abort: cannot pushrebase * onto remote bookmark master_bookmark: they have no common ancestor (glob)
  (rebase the stack onto master_bookmark first)
  [255]
however, we can create a new bookmark there, bypassing the hook
  $ hg push -r . --to newbookmark --create --pushvar ALLOW_LARGE_FILES=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark newbookmark (glob)
  creating remote bookmark newbookmark
then, it's not a pushrebase so we can move master_bookmark there
  $ hg push -r . --to master_bookmark --pushvar NON_FAST_FORWARD=true --pushvar ALLOW_LARGE_FILES=true
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  moving remote bookmark master_bookmark from * to * (glob)

pushing another bookmark to the same commit shouldn't require running that hook
  $ hg up -q "parents($Y)"
  $ hg push -r . --to other --create
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark other (glob)
  creating remote bookmark other
  $ hg up -q $Z
  $ hg push -r . --to yet_another --create
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark yet_another (glob)
  creating remote bookmark yet_another
