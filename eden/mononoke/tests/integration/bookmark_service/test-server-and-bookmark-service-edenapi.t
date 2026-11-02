# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ export ENABLE_BOOKMARK_CACHE=1
Setup repository

  $ REPONAME="repo" setup_common_config
setup basic repo with one bookmark
  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C
  > # bookmark: C master_bookmark
  > # bookmark: B stable
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

  $ start_and_wait_for_bookmark_service
  $ start_and_wait_for_mononoke_server_with_client_certs --use-remote-bookmark-cache true --remote-bookmark-cache-host-port localhost:$BOOKMARK_SERVICE_PORT
  $ setconfig pull.httphashprefix=1 pull.use-commit-graph=true clone.use-rust=true clone.use-commit-graph=true
  $ hg clone -q mono:repo --noupdate -q
  $ cd repo
  $ hg log -r "master_bookmark" -T '{desc}\n'
  C

  $ testtool_drawdag -R repo <<EOF
  >   G
  >  /
  > C-D-E
  >    \
  >     F
  > # exists: C $C
  > # bookmark: E master_bookmark
  > # bookmark: F feature
  > # bookmark: F feature2
  > EOF
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  E=3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd
  F=d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e
  G=68ebed8873951fb1c0148207109b41e5af281a42badd0d6d33611db709b15234

  $ wait_for_bookmark_service_update repo master_bookmark $E
  $ truncate -s 0 "$TESTTMP/bookmark-service.json"
  $ hg pull -q
  $ hg log -r "master_bookmark" -T '{desc}\n'
  E

  $ cat "$TESTTMP/bookmark-service.json" | summarize_scuba_json "Request complete" \
  >     .normal.method .normal.status .normal.repo .normal.bookmark .normal.error | jq -c
  {"bookmark":"master_bookmark","error":"","method":"get","repo":"repo","status":"SUCCESS"}
  {"bookmark":"master_bookmark","error":"","method":"get","repo":"repo","status":"SUCCESS"}

  $ hg debugapi -e bookmarks -i '[ "feature", "unknown"]'
  {"feature": "3ac82645e56c56b195da4c0a0c9b5baa081b2fae",
   "unknown": None}



  $ killandwait $BOOKMARK_SERVICE_PID

  $ hg debugapi -e bookmarks -i '[ "feature", "unknown"]'
  abort: Error fetching key None: SaplingRemoteAPI server returned an error with message: ServerError { message: "InternalError(InternalError(Failed to fetch HgId for bookmark: \"feature\". Err: internal error: BookmarkService::get failed with ThriftError*", code: 1 } (glob)
  [255]
