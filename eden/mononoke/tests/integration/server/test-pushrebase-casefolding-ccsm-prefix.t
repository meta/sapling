# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ setconfig push.edenapi=true
  $ INFINITEPUSH_NAMESPACE_REGEX='^scratch/.+$' setup_mononoke_config
  $ cd "$TESTTMP/mononoke-config"
  $ cat >> repos/repo/server.toml <<CONFIG
  > [[bookmarks]]
  > name="master_bookmark"
  > [[bookmarks]]
  > regex=".*"
  > hooks_skip_ancestors_of=["master_bookmark"]
  > CONFIG

  $ setup_common_hg_configs
  $ setconfig remotenames.selectivepulldefault=master_bookmark,other
  $ cd $TESTTMP

  $ configure dummyssh
  $ enable amend

setup repo
  $ hginit_treemanifest repo
  $ cd repo
  $ testtool_drawdag -R repo <<EOF
  > A-B
  > A-C
  > # modify: A "somefile" "somefile\n"
  > # modify: C "existing/caseconflict" "caseconflict\n"
  > # modify: C "existing/CaseConflict" "caseconflict\n"
  > # bookmark: B master_bookmark
  > # bookmark: C other
  > EOF
  A=17e539a4e3bfe31553dff43e1f990edf4403d4f8ec1a056fa9267d6b91263a48
  B=8108bb23c941f8af782360c3554489f9bfea96b3bcf1473eeba1c810690a2ba6
  C=fc6bd7272646f0fb63ea9f78f055c9493cc2e841dce663474ff1384e8cdecb95

Import and start mononoke
  $ cd "$TESTTMP"
  $ mononoke
  $ wait_for_mononoke
clone
  $ hg clone -q mono:repo repo2 --noupdate
  $ cd repo2
  $ enable pushrebase commitcloud
  $ setconfig infinitepush.server=false infinitepush.branchpattern='re:scratch/.+'

The `other` bookmark points at C, which already carries the case conflict
existing/caseconflict vs existing/CaseConflict. A push on top of it that only adds
existing/case (a new name whose lowercased form is a strict prefix of the existing
lowercased name "caseconflict") introduces no new conflict and must land. The CCSM
check compares the directory's sharded maps {"case", "caseconflict"} and {"caseconflict"},
whose radix-trie node prefixes diverge at "case".
  $ hg up -q other
  $ echo case > existing/case
  $ hg add existing/case
  $ hg commit -qm addcase

FIXME: BUG! The pre-existing conflict is reported as new (with no previous values) and
the push is rejected, because `ShardedMapV2Node::difference_stream` drops the parent map's
value for "caseconflict" while that node still carries the prefix "conflict".
  $ hg push -r . --to other 2>&1 | grep -v "^edenapi:"
  pushing rev * to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark other (glob)
  pushrebasing stack (*, *] (1 commit) to remote bookmark other (glob)
  abort: Server error: invalid request: Case conflict found in *: existing/CaseConflict conflicts with existing/caseconflict (glob)
