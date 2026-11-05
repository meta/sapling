# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Set up local hgrc and Mononoke config.
  $ setup_common_config
  $ cd $TESTTMP

  $ testtool_drawdag -R repo --print-hg-hashes << EOF
  > A-B
  > # bookmark: B master_bookmark
  > EOF
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  B=80521a640a0c8f51dcc128c2658b224d595840ac

  $ start_and_wait_for_mononoke_server

Remove edenapi.url, so the SLAPI server comes from the mono: path. The test
server certificate is only valid for localhost.
  $ cat >> "$HGRCPATH" << EOF
  > [edenapi]
  > url=
  > [schemes]
  > mono=mononoke://localhost:$MONONOKE_SOCKET/{1}
  > EOF

  $ hg clone -q mono:repo client
  $ cd client
  $ hg log -r master_bookmark -T '{node}\n'
  80521a640a0c8f51dcc128c2658b224d595840ac

The repo's client uses https://host:port/slapi/, and the repo name Sapling derives
from the path.
  $ hg debugapi -e url
  "https://localhost:$LOCAL_PORT/slapi/"
  $ hg debugapi -e bookmarks -i '["master_bookmark"]'
  {"master_bookmark": "80521a640a0c8f51dcc128c2658b224d595840ac"}

So does a client built for a path given on the command line.
  $ hg debugapi mono:repo -e url
  "https://localhost:$LOCAL_PORT/slapi/"
  $ hg debugapi mono:repo -e bookmarks -i '["master_bookmark"]'
  {"master_bookmark": "80521a640a0c8f51dcc128c2658b224d595840ac"}

Pull works with no edenapi.url.
  $ hg pull -q
  $ hg log -r master_bookmark -T '{node}\n'
  80521a640a0c8f51dcc128c2658b224d595840ac

An explicit edenapi.url wins over the path. The slapi.url_host_mismatch
counter records when it names a different server.
  $ hg debugapi -e url --config edenapi.url=https://localhost:$MONONOKE_SOCKET/edenapi/ --config devel.print-metrics=slapi.
  "https://localhost:$LOCAL_PORT/edenapi/"
  $ hg debugapi -e url --config edenapi.url=https://other.example.com/edenapi/ --config devel.print-metrics=slapi.
  "https://other.example.com/edenapi/"
  slapi.url_host_mismatch: 1

experimental.slapi-url-from-path=false turns the derivation off. Without
edenapi.url the repo then has no SLAPI client, which its lazy commit graph needs.
  $ hg debugshell -c 'ui.write("%s\n" % (repo.nullableedenapi is None))'
  False
  $ hg debugshell -c 'ui.write("%s\n" % (repo.nullableedenapi is None))' --config experimental.slapi-url-from-path=false
  abort: When constructing alloc::boxed::Box<dyn commits_trait::DagCommits + core::marker::Send> from dyn storemodel::StoreInfo, after being ignored by ["10-git-commits"], "20-hgcommits" reported error
  
  Caused by:
      The commit graph requires a remote peer but the repo does not have one.
  [255]
