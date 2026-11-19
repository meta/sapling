# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

setup configuration
  $ base_snapshot_repo_setup client1
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  $ cd client1
  $ echo "new file content" > new_file
  $ hg add new_file
  $ SNAPSHOT=$(hg snapshot create | head -n 1 | awk '{print $NF}')

  $ printf "uploaded to the snapshot\n" > "$TESTTMP/content"
  $ SHA1=$(sha1sum "$TESTTMP/content" | cut -d ' ' -f 1)
  $ upload_file() {
  >   sslcurlas client0 -s -o "$TESTTMP/response" -w "%{http_code}\n" -X PUT \
  >     --data-binary "@$TESTTMP/content" \
  >     "https://localhost:$MONONOKE_SOCKET/edenapi/repo/upload/file/sha1/$SHA1?content_size=25&$1"
  > }
  $ error_message() {
  >   jq -r .message "$TESTTMP/response"
  > }

No endpoint writes the bubble hg mapping yet, so map an hg id to the snapshot's
bubble directly. The snapshot is the first one in this repo, so its bubble is 1:
  $ SNAPSHOT_HG=1234567890abcdef1234567890abcdef12345678
  $ BUBBLE=$(sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT bubble_id FROM ephemeral_bubble_changeset_mapping WHERE cs_id = X'$SNAPSHOT'")
  $ echo "$BUBBLE"
  1
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "INSERT INTO ephemeral_bubble_hg_mapping (repo_id, hg_cs_id, bubble_id, bcs_id) VALUES (0, X'$SNAPSHOT_HG', $BUBBLE, X'$SNAPSHOT')"

With the bubble hg mapping off, naming a snapshot is rejected even though it is
mapped, and nothing is stored in its bubble:
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:enable_bubble_hg_mapping": false}}
  > EOF
  $ force_update_configerator
  $ upload_file "snapshot=$SNAPSHOT_HG"
  400
  $ error_message
  Bubble hg mapping is disabled for repo repo, so snapshots cannot be named by hg id
  $ mononoke_admin ephemeral-store -R repo list -i "$SNAPSHOT" | grep -c "alias.sha1.$SHA1"
  0
  [1]

  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:enable_bubble_hg_mapping": true}}
  > EOF
  $ force_update_configerator

Uploading a file that names a snapshot by hg id stores it in that snapshot's bubble:
  $ upload_file "snapshot=$SNAPSHOT_HG"
  200
  $ mononoke_admin ephemeral-store -R repo list -i "$SNAPSHOT" | grep -c "alias.sha1.$SHA1"
  1

Naming a snapshot that does not exist is rejected:
  $ upload_file "snapshot=abcdefabcdefabcdefabcdefabcdefabcdefabcd"
  404
  $ error_message
  Snapshot abcdefabcdefabcdefabcdefabcdefabcdefabcd not found

Naming a snapshot by a malformed hg id is rejected:
  $ upload_file "snapshot=$SNAPSHOT"
  400
  $ error_message
  invalid sha1 input: need exactly 40 hex digits

Naming both a snapshot and a bubble is rejected:
  $ upload_file "snapshot=$SNAPSHOT_HG&bubble_id=$BUBBLE"
  400
  $ error_message
  Name either a bubble or a snapshot, not both
