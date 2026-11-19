# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ setconfig ui.ignorerevnum=false

  $ setconfig pull.use-commit-graph=true clone.use-rust=true clone.use-commit-graph=true

Set up local hgrc and Mononoke config, with commit cloud, http pull and upload.
  $ export READ_ONLY_REPO=1
  $ export LOG=pull
  $ INFINITEPUSH_ALLOW_WRITES=true \
  >   setup_common_config
  $ cd $TESTTMP
  $ cat >> $HGRCPATH <<EOF
  > [commitcloud]
  > servicetype = local
  > [mutation]
  > record = True
  > [pull]
  > httphashprefix = true
  > EOF
Custom smartlog
  $ function smartlog {
  >  hg log -G -T "{node|short} '{desc|firstline}' {join(mutations % '(Rewritten using {operation} into {join(successors % \'{node|short}\', \', \')})', ' ')}" --hidden
  > }

Initialize test repo.
  $ testtool_drawdag -R repo --print-hg-hashes << EOF
  > base_commit
  > # bookmark: base_commit master_bookmark
  > EOF
  base_commit=eb9c16dd0f62fa641290156c4988cf35c48c5fbe

Import and start mononoke
  $ start_and_wait_for_mononoke_server
  $ hg clone -q mono:repo client1 --noupdate
  $ hg clone -q mono:repo client2 --noupdate

Test mutations on client 1
  $ cd client1
  $ hg up eb9c16dd0f62 -q
  $ hg cloud join -q
  $ mkcommitedenapi A
  $ hg log -T{node} -r .
  88be7633b9a1204e3f5746bb619e8acacf4bb742 (no-eol)
  $ hg cloud upload -q
  $ hg debugapi -e commitmutations -i '["929f2b9071cf032d9422b3cce9773cbe1c574822"]'
  []
  $ hg metaedit -r . -m new_message
  $ hg log -T{node} -r .
  7dfd038512d12efc5be9148650e7043f3516f458 (no-eol)
  $ hg cloud upload -q
  $ hg debugapi -e commitmutations -i '["7dfd038512d12efc5be9148650e7043f3516f458"]'
  [{"op": "metaedit",
    "tz": 0,
    "time": 0,
    "user": [116,
             101,
             115,
             116],
    "split": [],
    "extras": [],
    "successor": bin("7dfd038512d12efc5be9148650e7043f3516f458"),
    "predecessors": [bin("88be7633b9a1204e3f5746bb619e8acacf4bb742")]}]
  $ hg debugapi -e commitmutations -i '["929f2b9071cf032d9422b3cce9773cbe1c574822"]'
  []
Test phases from commitgraph
  $ hg debugapi -e commitgraph -i '["7dfd038512d12efc5be9148650e7043f3516f458", "88be7633b9a1204e3f5746bb619e8acacf4bb742"]' -i '[]' --sort
  [{"hgid": bin("88be7633b9a1204e3f5746bb619e8acacf4bb742"),
    "parents": [bin("eb9c16dd0f62fa641290156c4988cf35c48c5fbe")],
    "is_draft": True},
   {"hgid": bin("eb9c16dd0f62fa641290156c4988cf35c48c5fbe"),
    "parents": [],
    "is_draft": True},
   {"hgid": bin("7dfd038512d12efc5be9148650e7043f3516f458"),
    "parents": [bin("eb9c16dd0f62fa641290156c4988cf35c48c5fbe")],
    "is_draft": True}]
  $ hg debugapi -e commitmutations -i '["7dfd038512d12efc5be9148650e7043f3516f458", "88be7633b9a1204e3f5746bb619e8acacf4bb742"]'
  [{"op": "metaedit",
    "tz": 0,
    "time": 0,
    "user": [116,
             101,
             115,
             116],
    "split": [],
    "extras": [],
    "successor": bin("7dfd038512d12efc5be9148650e7043f3516f458"),
    "predecessors": [bin("88be7633b9a1204e3f5746bb619e8acacf4bb742")]}]
  $ smartlog
  @  7dfd038512d1 'new_message'
  │
  │ x  88be7633b9a1 'A' (Rewritten using metaedit into 7dfd038512d1)
  ├─╯
  o  eb9c16dd0f62 'base_commit'
  


Test how they are propagated to client 2
  $ cd ../client2
  $ hg debugchangelog --migrate lazy
  $ hg pull -r 7dfd038512d1 -q
  DEBUG pull::httpbookmarks: edenapi fetched bookmarks: {'master_bookmark': 'eb9c16dd0f62fa641290156c4988cf35c48c5fbe'}
  DEBUG pull::fastpath: master_bookmark: eb9c16dd0f62fa641290156c4988cf35c48c5fbe (unchanged)
  DEBUG pull::httphashlookup: edenapi hash lookups: ['7dfd038512d12efc5be9148650e7043f3516f458']
  DEBUG pull::httpgraph: edenapi fetched 1 graph nodes
  DEBUG pull::httpgraph: edenapi fetched graph with known 1 draft commits
  $ hg pull -r 88be7633b9a1 -q
  DEBUG pull::httpbookmarks: edenapi fetched bookmarks: {'master_bookmark': 'eb9c16dd0f62fa641290156c4988cf35c48c5fbe'}
  DEBUG pull::fastpath: master_bookmark: eb9c16dd0f62fa641290156c4988cf35c48c5fbe (unchanged)
  DEBUG pull::httphashlookup: edenapi hash lookups: ['88be7633b9a1204e3f5746bb619e8acacf4bb742']
  DEBUG pull::httpgraph: edenapi fetched 1 graph nodes
  DEBUG pull::httpgraph: edenapi fetched graph with known 1 draft commits
  $ smartlog
  x  88be7633b9a1 'A' (Rewritten using metaedit into 7dfd038512d1)
  │
  │ o  7dfd038512d1 'new_message'
  ├─╯
  o  eb9c16dd0f62 'base_commit'
  
