# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Set up local hgrc and Mononoke config.
  $ setup_common_config
  $ setup_configerator_configs
  $ cd $TESTTMP

Populate test repo. M merges the draft branch D-E into C, and master has moved
on to X. The release branch R is public, and S is a draft commit on it.
  $ testtool_drawdag -R repo --print-hg-hashes << EOF
  > X M
  > |/|
  > C E
  > | |
  > | D
  > |/
  > B S
  > | |
  > | R
  > |/
  > A
  > # bookmark: X master_bookmark
  > # bookmark: R release
  > EOF
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  B=80521a640a0c8f51dcc128c2658b224d595840ac
  C=d3b399ca8757acdb81c3681b052eb978db6768d8
  D=66b90ab43bf12b5fa49c4940eaaf3edd70ae02ea
  E=c3ec762600b18757898ea4dbae0359ee7e55aec9
  M=18e91daf45943589b0197acf365a2165846c60e3
  R=9319d9bdb2e898d562608a084576ccaa77f5d409
  S=89fc2ef4030447c9c0f820bfef29008576ee019e
  X=3938f7d573414886e4383cea17634d5213ca4fe3

Start up SaplingRemoteAPI server.
  $ setup_mononoke_config
  $ start_and_wait_for_mononoke_server
  $ cd $TESTTMP
  $ hg clone -q mono:repo repo --noupdate
  $ cd repo
  $ hg pull -q -r $M -r $S
  $ hg log -r $M -T '{p1node} {p2node}\n'
  d3b399ca8757acdb81c3681b052eb978db6768d8 c3ec762600b18757898ea4dbae0359ee7e55aec9

`base` must be the merge base of the head and the bookmark, which for M is C.
B is rejected:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$M'" -i "'$B'"
  {"data": {"Err": {"code": 0,
                    "message": "invalid request: bookmark 'master_bookmark' has moved incompatibly with this push: the merge base of 'master_bookmark' and 18e91daf45943589b0197acf365a2165846c60e3 is d3b399ca8757acdb81c3681b052eb978db6768d8, but the push is based on 80521a640a0c8f51dcc128c2658b224d595840ac. Pull and retry"}}}

With C, only M descends from the base, so only M is rebased. D and E land
unchanged as its second parent:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$M'" -i "'$C'"
  {"data": {"Ok": {"new_head": bin("eb2adec8ae8409210514216dec3122b30daf7232"),
                   "old_to_new_hgids": {bin("18e91daf45943589b0197acf365a2165846c60e3"): bin("eb2adec8ae8409210514216dec3122b30daf7232")}}}}

  $ hg pull -q
  $ hg log -G -T '{desc} {remotenames}\n' -r "sort(all(),topo)"
  o    M
  ├─╮
  │ │ o  M remote/master_bookmark
  │ ╭─┤
  │ o │  E
  │ │ │
  │ o │  D
  │ │ │
  │ │ o  X
  ├───╯
  o │  C
  ├─╯
  o  B
  │
  │ o  S
  │ │
  │ o  R
  ├─╯
  o  A
  
  $ hg log -r master_bookmark -T '{p1node} {p2node}\n'
  3938f7d573414886e4383cea17634d5213ca4fe3 c3ec762600b18757898ea4dbae0359ee7e55aec9
  $ hg files -r master_bookmark
  A
  B
  C
  D
  E
  M
  X

A head that is already on the bookmark leaves nothing to land:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$X'" -i "'$X'"
  {"data": {"Err": {"code": 0,
                    "message": "invalid request: nothing to land: 3938f7d573414886e4383cea17634d5213ca4fe3 is already an ancestor of 'master_bookmark'"}}}

S is on the release branch. Its merge base with master is A, and landing from
there would rebase R, which is public:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$S'" -i "'$A'"
  {"data": {"Err": {"code": 0,
                    "message": "invalid request: cannot rebase public commits: 9319d9bdb2e898d562608a084576ccaa77f5d409"}}}

Based on R instead, the push is rejected as not based on the merge base:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$S'" -i "'$R'"
  {"data": {"Err": {"code": 0,
                    "message": "invalid request: bookmark 'master_bookmark' has moved incompatibly with this push: the merge base of 'master_bookmark' and 89fc2ef4030447c9c0f820bfef29008576ee019e is 20ca2a4749a439b459125ef0f6a4f26e88ee7538, but the push is based on 9319d9bdb2e898d562608a084576ccaa77f5d409. Pull and retry"}}}
