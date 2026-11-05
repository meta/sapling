# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Second-parent roots are forbidden, as in production.
  $ export FORBID_P2_ROOT_REBASES=1
  $ setup_common_config
  $ cd $TESTTMP

  $ testtool_drawdag -R repo --print-hg-hashes << EOF
  > C
  > |
  > B
  > |
  > A
  > # bookmark: C master_bookmark
  > EOF
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  B=80521a640a0c8f51dcc128c2658b224d595840ac
  C=d3b399ca8757acdb81c3681b052eb978db6768d8

  $ start_and_wait_for_mononoke_server
  $ hg clone -q mono:repo repo
  $ cd repo
  $ setconfig push.edenapi=true
  $ setconfig experimental.slapi-bookmark-push=true
  $ setconfig experimental.edenapi-push-merges=true

`push` shows the push's output, and the commands it sent over the wire protocol.
  $ push() {
  >   hg push "$@" --debug > "$TESTTMP/push.log" 2>&1
  >   local rc=$?
  >   grep -E '^(sending [a-z]+ command|fallback reason|pushing|pushrebasing|moving remote|creating remote|updated remote|updating bookmark|abort)' "$TESTTMP/push.log"
  >   return $rc
  > }
  $ log() {
  >   hg log -G -T '{desc} [{phase}] {remotenames}' -r "$@"
  > }
`commit NAME [PARENT...]` commits file NAME on the first parent, merging the
second, and sets $NAME to the new commit.
  $ commit() {
  >   local name="$1"
  >   hg up -q "$2"
  >   if [ -n "$3" ]; then hg merge -q "$3"; fi
  >   echo "$name" > "$name" && hg add -q "$name" && hg ci -qm "$name"
  >   eval "$name=$(hg log -r . -T '{node}')"
  > }

A linear push goes through SLAPI.
  $ commit L1 master_bookmark
  $ push --to master_bookmark
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark (glob)
  pushrebasing stack (*, *] (1 commit) to remote bookmark master_bookmark (glob)
  updated remote bookmark master_bookmark to * (glob)

A merge of a draft branch into master. Master moves on before the push, so the
merge, the only commit between its merge base with master and itself, is
rebased; the draft branch lands as the merge's second parent. The push goes
through SLAPI. The pull of the landed merge that follows cannot take
the fast path, as the draft branch is already in the local graph, so it opens a
wire protocol connection (`hello`); the pull itself goes through SLAPI.
  $ commit D $A
  $ commit MD master_bookmark $D
  $ commit E master_bookmark
  $ hg push -q --to master_bookmark
  $ hg up -q $MD
  $ push --to master_bookmark
  pushing rev 7e1941e2c430 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  pushrebasing stack (66fa457385fb, 7e1941e2c430] (1 commit) to remote bookmark master_bookmark
  sending hello command
  updated remote bookmark master_bookmark to 61ec3d7cd010
  $ log "$A::"
  @    MD [public] remote/master_bookmark
  ├─╮
  │ o  D [public]
  │ │
  o │  E [public]
  │ │
  o │  L1 [public]
  │ │
  o │  C [public]
  │ │
  o │  B [public]
  ├─╯
  o  A [public]
  
  $ hg log -r "predecessors(.)" --hidden -T '{node|short} {desc}\n'
  61ec3d7cd010 MD
  7e1941e2c430 MD

A merge of two draft branches. F and the merge descend from the merge base, so
both are rebased; G lands as it is.
  $ commit F master_bookmark
  $ commit G $B
  $ commit MFG $F $G
  $ commit H master_bookmark
  $ hg push -q --to master_bookmark
  $ hg up -q $MFG
  $ push --to master_bookmark
  pushing rev dac646e5b5a9 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  pushrebasing stack (61ec3d7cd010, dac646e5b5a9] (2 commits) to remote bookmark master_bookmark
  sending hello command
  updated remote bookmark master_bookmark to 6cc3000aa704
  $ hg log -r "parents(.)" -T '{desc} [{phase}]\n'
  G [public]
  F [public]
  $ hg log -r "predecessors(.) + predecessors(p1(.))" --hidden -T '{node|short} {desc}\n'
  6cc3000aa704 MFG
  dac646e5b5a9 MFG
  99721ba08318 F
  6b5b49bbe33e F

A merge of master into a branch has master as its second parent, so its merge
base with master, which the stack is rebased from, is a second parent. That is
forbidden:
  $ commit J $A
  $ commit MJ $J master_bookmark
  $ push --to master_bookmark
  pushing rev c67d4217bb93 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  pushrebasing stack (6cc3000aa704, c67d4217bb93] (1 commit) to remote bookmark master_bookmark
  abort: Server error: invalid request: Pushrebase failed: Forbid pushrebase because root (6cc3000aa704180b569389da47d03ec7f31c55e1) is not a p1 of master_bookmark bookmark
  [255]

--no-rebase moves the bookmark to a merge:
  $ commit K $A
  $ commit MK master_bookmark $K
  $ push --to master_bookmark --no-rebase
  pushing rev 526a0bdaf212 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  moving remote bookmark master_bookmark from 6cc3000aa704 to 526a0bdaf212
  $ log master_bookmark
  @    MK [public] remote/master_bookmark
  ├─╮
  │ │
  ~ ~

A stack on a public branch other than master would rebase that branch's public
commits, so the client refuses to push it:
  $ commit R $A
  $ hg push -q -r $R --to release --create
  $ commit S $R
  $ push --to master_bookmark
  pushing rev 27ae231999b0 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  abort: cannot rebase public changesets: 568d517e80cb
  [255]
  $ hg log -r master_bookmark -T '{desc}\n'
  MK

-B pushes a merge:
  $ commit N $A
  $ commit MN master_bookmark $N
  $ hg bookmark foo
  $ push -B foo
  pushing rev e9bbc201c7e6 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark foo
  creating remote bookmark foo
  $ log foo
  @    MN [public] remote/foo
  ├─╮
  │ │
  ~ ~

With `experimental.edenapi-push-merges` off, which is the default, merges are
pushed over the wire protocol, except with --no-rebase, which only SLAPI does.
  $ commit P $A
  $ commit MP master_bookmark $P
  $ push --to master_bookmark --no-rebase --config experimental.edenapi-push-merges=false
  abort: merge commit is not supported by EdenApi push yet
  [255]
  $ push --to master_bookmark --config experimental.edenapi-push-merges=false
  fallback reason: merge commit is not supported by EdenApi push yet
  sending hello command
  pushing rev 575a01b25bcd to destination mono:repo bookmark master_bookmark
  sending unbundle command
  updating bookmark master_bookmark
  $ commit Q $A
  $ commit MQ foo $Q
  $ hg bookmark -f foo
  $ push -B foo --config experimental.edenapi-push-merges=false
  fallback reason: merge commit is not supported by EdenApi push yet
  pushing to mono:repo
  sending hello command
  sending unbundle command
  updating bookmark foo
