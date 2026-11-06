# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

Modern sync from a source whose readers use augmented manifests instead of Hg
manifests.

  $ . "${TEST_FIXTURES}/library.sh"

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
  >          "bypass_readonly": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA","SERVICE_IDENTITY:server", "X509_SUBJECT_NAME:CN=localhost,O=Mononoke,C=US,ST=CA", "X509_SUBJECT_NAME:CN=client0,O=Mononoke,C=US,ST=CA"]
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

  $ REPOID=0 REPONAME=orig ACL_NAME=orig setup_common_config "blob_files"
  $ REPOID=1 REPONAME=dest ACL_NAME=dest setup_common_config "blob_files"
  $ start_and_wait_for_mononoke_server

  $ hg clone -q mono:orig orig
  $ cd orig
  $ drawdag << EOS
  > A # A/dir1/dir2/first = abc\n
  > EOS
  $ hg goto $A -q
  $ hg push -r . --to master_bookmark -q --create

The second commit edits, deletes and adds files, including an executable and a
symlink.
  $ echo def > dir1/dir2/first
  $ hg rm -q A
  $ mkdir -p dir1/new
  $ echo exec > dir1/new/exec && chmod +x dir1/new/exec
  $ ln -s ../dir2 dir1/new/link
  $ hg commit -Aqm B
  $ hg push -r . --to master_bookmark -q

Turn on augmented manifest reads for modern sync only: the replica keeps
reading Hg manifests, as it does in production. Keep a copy of the source's Hg
manifests and remove them: nothing may read them.
  $ jq '.bools["scm/mononoke:hgmanifest_read_from_augmented"] = true' "$MONONOKE_JUST_KNOBS_OVERRIDES_PATH" \
  >   > "$(dirname "$MONONOKE_JUST_KNOBS_OVERRIDES_PATH")/modern_sync_just_knobs.json"
  $ mkdir "$TESTTMP/orig-hgmanifests"
  $ mv "$TESTTMP"/blobstore/blobs/blob-repo0000.hgmanifest.* "$TESTTMP/orig-hgmanifests"

Modern sync reads the source's trees from its augmented manifests, and the
replica gets the same trees the source's Hg manifests held.
  $ ( COMMON_ARGS=("${COMMON_ARGS[@]/#just_knobs.json/modern_sync_just_knobs.json}")
  >   mononoke_modern_sync "" sync-once orig dest --start-id 0 > "$TESTTMP/sync.out" 2>&1 ); echo "exit $?"
  exit 0
  $ grep -c 'Blob is missing: hgmanifest' "$TESTTMP/sync.out" || true
  0
  $ mononoke_admin mutable-counters --repo-name orig get modern_sync
  Some(2)
  $ for f in "$TESTTMP"/orig-hgmanifests/*; do
  >   cmp -s "$f" "$TESTTMP/blobstore/blobs/$(basename "$f" | sed 's/repo0000/repo0001/')" || echo "$f"
  > done | wc -l
  0
