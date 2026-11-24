# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Without the mirror_upload tier the replica takes contents but no trees, so a
sync uploads contents and then fails, like one that restarts mid-entry.

  $ cat >> "$ACL_FILE" << ACLS
  > {
  >   "repos": {
  >     "orig": {
  >       "actions": {
  >         "read": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "write": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "bypass_readonly": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"]
  >       }
  >     },
  >     "dest": {
  >       "actions": {
  >         "read": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "write": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "bypass_readonly": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"]
  >       }
  >     }
  >   }
  > }
  > ACLS

  $ REPOID=0 REPONAME=orig ACL_NAME=orig setup_common_config
  $ REPOID=1 REPONAME=dest ACL_NAME=dest setup_common_config

  $ start_and_wait_for_mononoke_server

  $ hg clone -q mono:orig orig
  $ cd orig
  $ drawdag << EOS
  > F # F/dir1/dir2/sixth = abcdefgh\n
  > |
  > E # E/dir1/dir2/fifth = abcdefg\n
  > |
  > D # D/dir1/dir2/forth = abcdef\n
  > |
  > C # C/dir1/dir2/third = abcde\n (copied from dir1/dir2/first)
  > |
  > B # B/dir1/dir2/second = abcd\n
  > |
  > A # A/dir1/dir2/first = abc\n
  > EOS

The first read of the log (10 entries in tests) holds only moves of another
bookmark, so the master_bookmark entries come in the second read.

  $ hg push -r $A --to unrelated -q --create
  $ hg push -r $B --to unrelated -q
  $ for _ in 1 2 3 4; do
  >   quiet mononoke_admin bookmarks -R orig set unrelated $A
  >   quiet mononoke_admin bookmarks -R orig set unrelated $B
  > done
  $ mononoke_admin bookmarks -R orig log unrelated | wc -l
  10

  $ hg push -r $A --to master_bookmark -q --create
  $ hg push -r $E --to master_bookmark -q

The first sync uploads the 10 contents of A to E, then fails on trees.

  $ mononoke_modern_sync "" sync-once orig dest --start-id 0 > "$TESTTMP/first.log" 2>&1
  [1]
  $ grep -oE "Uploaded [0-9]+ contents" "$TESTTMP/first.log" | awk '{n += $2} END {print n}'
  10
  $ grep -o "Failed to upload trees" "$TESTTMP/first.log" | uniq
  Failed to upload trees

F brings two contents the replica doesn't have. Grant the tier and restart
the server.

  $ hg push -r $F --to master_bookmark -q

  $ cat > "$ACL_FILE" << ACLS
  > {
  >   "repos": {
  >     "orig": {
  >       "actions": {
  >         "read": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "write": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "bypass_readonly": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"]
  >       }
  >     },
  >     "dest": {
  >       "actions": {
  >         "read": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "write": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"],
  >         "bypass_readonly": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"]
  >       }
  >     }
  >   },
  >   "tiers": {
  >     "mirror_commit_upload": {
  >       "actions": {
  >         "mirror_upload": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"]
  >       }
  >     }
  >   }
  > }
  > ACLS
  $ killandwait $MONONOKE_PID
  $ start_and_wait_for_mononoke_server

The restarted sync looks up the contents of the first entries it syncs, even
though its first read synced nothing: it skips the 10 the failed sync uploaded
and uploads only F's 2.

  $ mononoke_modern_sync "" sync-once orig dest --start-id 0 > "$TESTTMP/second.log" 2>&1
  $ grep -oE "Skipping [0-9]+ of [0-9]+ contents" "$TESTTMP/second.log" | awk '{n += $2} END {print n}'
  10
  $ grep -oE "Uploaded [0-9]+ contents" "$TESTTMP/second.log" | awk '{n += $2} END {print n}'
  2

The replica has every commit, with the contents of both syncs.

  $ mononoke_admin bookmarks -R dest log master_bookmark -S hg | cut -d' ' -f1
  13
  12
  11
  $ cd "$TESTTMP"
  $ hg clone -q mono:dest dest
  $ cd dest
  $ hg log -r master_bookmark -T '{desc}\n'
  F
  $ cat dir1/dir2/third dir1/dir2/sixth
  abcde
  abcdefgh
