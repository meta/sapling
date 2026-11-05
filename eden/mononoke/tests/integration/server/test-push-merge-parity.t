# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

The same pushes over the wire protocol and over SLAPI land the same way, or
fail the same way. The block_files hook on master_bookmark rejects files whose
names start with "blocked_".

  $ . "${TEST_FIXTURES}/library.sh"
  $ export FORBID_P2_ROOT_REBASES=1
  $ hook_test_setup block_files <(
  >   cat <<CONF
  > config_json='''{"block_patterns": ["^blocked_"]}'''
  > CONF
  > )

  $ wire() {
  >   hg push "$@" --config push.edenapi=false
  > }
  $ slapi() {
  >   hg push "$@" --config push.edenapi=true --config experimental.edenapi-push-merges=true
  > }
`commit NAME PARENT [OTHER]` commits file NAME on PARENT, merging OTHER, and
sets $NAME to the new commit. `remove NAME PARENT FILE` commits the removal of
FILE.
  $ commit() {
  >   local name="$1"
  >   hg up -q "$2"
  >   if [ -n "$3" ]; then hg merge -q "$3"; fi
  >   echo "$name" > "$name" && hg add -q "$name" && hg ci -qm "$name"
  >   eval "$name=$(hg log -r . -T '{node}')"
  > }
  $ remove() {
  >   hg up -q "$2" && hg rm -q "$3" && hg ci -qm "$1"
  >   eval "$1=$(hg log -r . -T '{node}')"
  > }
`landed [NAME]` shows the parents of master_bookmark, and whether the second is
commit NAME, unchanged.
  $ landed() {
  >   hg pull -q
  >   hg log -r "parents(master_bookmark)" -T '{desc} {phase}\n'
  >   if [ -n "$1" ] && [ "$(hg log -r 'p2(master_bookmark)' -T '{node}')" = "${!1}" ]; then
  >     echo "$1 is unchanged"
  >   fi
  > }
  $ A=$(hg log -r 'desc(a)' -T '{node}')

A public release branch from A. Its first commit adds a file that the hook
rejects and its second removes it again, so merging the branch does not add
the file.
  $ commit blocked_release $A
  $ remove R1 . blocked_release
  $ hg push -q -r . --to release --create
  $ hg pull -q -B release
  $ hg log -r "$A::release" -T '{desc} {phase} {remotenames}\n'
  a public 
  blocked_release public 
  R1 public remote/release

(a) A draft commit on the release branch, pushed to master_bookmark. Landing
it would rebase the public release commits, so both protocols refuse:
  $ commit S $R1
  $ wire -r $S --to master_bookmark
  pushing rev 4167538d3854 to destination mono:repo bookmark master_bookmark
  searching for changes
  remote: Command failed
  remote:   Error:
  remote:     Pushrebase failed: No common pushrebase root for master_bookmark, all possible roots: {ChangesetId(Blake2(8f8e1ba11fdf40b1030dba994a53cc32ce9e0d1dc8d81c8b50bd18313174264c))}
  remote: 
  remote:     Caused by:
  remote:         No common pushrebase root for master_bookmark, all possible roots: {ChangesetId(Blake2(8f8e1ba11fdf40b1030dba994a53cc32ce9e0d1dc8d81c8b50bd18313174264c))}
  abort: unexpected EOL, expected netstring digit
  [255]
  $ slapi -r $S --to master_bookmark
  pushing rev 4167538d3854 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  abort: cannot rebase public changesets: bd29c004c35c, f46406d81d71
  [255]

(b) A merge of the public release branch into master_bookmark, after
master_bookmark moved on. Both protocols rebase the merge onto
master_bookmark and land the release commits unchanged. They are not checked
by hooks, so the file in them is not rejected:
  $ commit MW master_bookmark $R1
  $ commit W1 master_bookmark
  $ slapi -q -r . --to master_bookmark
  $ wire -r $MW --to master_bookmark
  pushing rev 076b98060fc7 to destination mono:repo bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark
  $ landed R1
  W1 public
  R1 public
  R1 is unchanged
  $ commit R2 $R1
  $ hg push -q -r . --to release
  $ hg pull -q -B release
  $ commit MS master_bookmark $R2
  $ commit W2 master_bookmark
  $ slapi -q -r . --to master_bookmark
  $ slapi -r $MS --to master_bookmark
  pushing rev 0ab798c75615 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  pushrebasing stack (f50962bdce6c, 0ab798c75615] (1 commit) to remote bookmark master_bookmark
  updated remote bookmark master_bookmark to f4228d26790d
  $ landed R2
  W2 public
  R2 public
  R2 is unchanged

(c) A merge of a draft feature branch from A, after master_bookmark moved on.
Both protocols rebase the merge and land the feature branch unchanged:
  $ commit FW $A
  $ commit MFW master_bookmark $FW
  $ commit W3 master_bookmark
  $ slapi -q -r . --to master_bookmark
  $ wire -r $MFW --to master_bookmark
  pushing rev baa989ee03ed to destination mono:repo bookmark master_bookmark
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  updating bookmark master_bookmark
  $ landed FW
  W3 public
  FW public
  FW is unchanged
  $ commit FS $A
  $ commit MFS master_bookmark $FS
  $ commit W4 master_bookmark
  $ slapi -q -r . --to master_bookmark
  $ slapi -r $MFS --to master_bookmark
  pushing rev b514cade2a56 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  edenapi: queue 2 commits for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  pushrebasing stack (0cc4e3f3e189, b514cade2a56] (1 commit) to remote bookmark master_bookmark
  updated remote bookmark master_bookmark to e861f2c3441a
  $ landed FS
  W4 public
  FS public
  FS is unchanged

With the file the hook rejects in a commit on the draft feature branch, both
protocols check that commit and reject the push:
  $ commit blocked_feature $A
  $ remove F2 . blocked_feature
  $ commit MF master_bookmark $F2
  $ wire -r $MF --to master_bookmark
  pushing rev d689abd63d3e to destination mono:repo bookmark master_bookmark
  searching for changes
  remote: Command failed
  remote:   Error:
  remote:     hooks failed:
  remote:     block_files for 5079eaa7521c22af348acc88ec923f4d18d40a10: Blocked filename 'blocked_feature' matched block pattern '^blocked_'. This path is protected and your change must not modify it. To fix this, revert your changes to 'blocked_feature' so that it no longer appears in your diff, then re-submit.
  abort: unexpected EOL, expected netstring digit
  [255]
  $ slapi -r $MF --to master_bookmark
  pushing rev d689abd63d3e to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  pushrebasing stack (e861f2c3441a, d689abd63d3e] (1 commit) to remote bookmark master_bookmark
  abort: Server error: hooks failed:
    block_files for bf9a4b5d532d987b878ab5a6367e1b152321dfe606547f548021c8b4463edb21: Blocked filename 'blocked_feature' matched block pattern '^blocked_'. This path is protected and your change must not modify it. To fix this, revert your changes to 'blocked_feature' so that it no longer appears in your diff, then re-submit.
  [255]
  $ landed
  W4 public
  FS public
