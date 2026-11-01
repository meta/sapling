# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# The mutation store is first-writer-wins: once an entry for a successor is
# stored, a later, different entry for the same successor is ignored and the
# stored entry must be read back unchanged (see the `hg_mutation_info`
# insert_or_ignore and mutation/test/cycles.rs).
# The predecessor rows of an entry are keyed by (repo, successor, seq) only, so
# a later entry with MORE predecessors still leaves its extra rows behind in
# the table; the read ignores every row at or beyond the stored pred_count.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ INFINITEPUSH_ALLOW_WRITES=true setup_common_config
  $ cd $TESTTMP

  $ testtool_drawdag -R repo --print-hg-hashes << EOF
  > A-B-C
  > # bookmark: C master_bookmark
  > EOF
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  B=80521a640a0c8f51dcc128c2658b224d595840ac
  C=d3b399ca8757acdb81c3681b052eb978db6768d8
  $ A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  $ B=80521a640a0c8f51dcc128c2658b224d595840ac
  $ C=d3b399ca8757acdb81c3681b052eb978db6768d8

  $ start_and_wait_for_mononoke_server
  $ hg clone -q mono:repo client1 --noupdate
  $ cd client1

# Mutation entries reach the server through the `uploadchangesets` endpoint,
# which takes the entries beside the uploaded commits. This helper writes one
# such request: an (empty) commit to upload, and one mutation entry for an
# existing successor.
  $ cat > "$TESTTMP/upload.py" <<'EOF'
  > import hashlib, sys
  > message, op, successor, preds = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:]
  > null = "0" * 40
  > text = ("%s\nuser\n0 0\n\n%s" % (null, message)).encode()
  > node = hashlib.sha1(b"\0" * 40 + text).hexdigest()
  > changeset = {
  >     "parents": None,
  >     "manifestid": null,
  >     "user": list(b"user"),
  >     "time": 0,
  >     "tz": 0,
  >     "extras": [],
  >     "files": [],
  >     "message": list(message.encode()),
  > }
  > mutation = {
  >     "successor": successor,
  >     "predecessors": preds,
  >     "split": [],
  >     "op": op,
  >     "user": list(b"user"),
  >     "time": 0,
  >     "tz": 0,
  >     "extras": [],
  > }
  > print(repr([(node, changeset)]))
  > print(repr([mutation]))
  > EOF
  $ upload() {
  >   python3 "$TESTTMP/upload.py" "$@" > "$TESTTMP/req"
  >   hg debugapi -e uploadchangesets -i "$(sed -n 1p "$TESTTMP/req")" -i "$(sed -n 2p "$TESTTMP/req")" > /dev/null
  > }
  $ mutations() {
  >   printf '%s\n' "$(hg debugapi -e commitmutations -i "['$1']" | tr -d ' \n' |
  >     sed -n 's/.*{"op":"\([a-z]*\)".*"predecessors":\(.*\)}].*/op=\1 preds=\2/p')"
  > }

# The first client reports that C was amended from A, and a second client
# reports a different history for the same commit C: a fold of A and B.
  $ upload "first upload" amend "$C" "$A"
  $ upload "second upload" fold "$C" "$A" "$B"

# The store kept the first entry: one predecessor, operation `amend`
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select hex(successor), pred_count, op from hg_mutation_info"
  D3B399CA8757ACDB81C3681B052EB978DB6768D8|1|amend

# The ignored second entry still left its extra predecessor row behind, because
# the predecessor rows are keyed by (repo, successor, seq) alone
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select seq, hex(predecessor) from hg_mutation_preds order by seq"
  0|20CA2A4749A439B459125EF0F6A4F26E88EE7538
  1|80521A640A0C8F51DCC128C2658B224D595840AC

# Reading the mutation history of C ignores that leaked row: the client is
# served the stored first entry, the `amend` from A alone.
  $ mutations "$C"
  op=amend preds=[bin("20ca2a4749a439b459125ef0f6a4f26e88ee7538")]
