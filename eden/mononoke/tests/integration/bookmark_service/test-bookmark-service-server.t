# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ REPONAME="repo" setup_common_config
setup basic repo with one bookmark
  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C
  > # bookmark: C main
  > # bookmark: B stable
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

  $ start_and_wait_for_bookmark_service
  $ bookmark_service_client status
  $ wait_for_bookmark_service_update repo main $C
test get method
  $ bookmark_service_client get --repo-name repo main
  e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
move bookmark forward and expect the cache to pick it up
  $ testtool_drawdag -R repo <<EOF
  >   G
  >  /
  > C-D-E
  >    \
  >     F
  > # exists: C $C
  > # bookmark: E main
  > # bookmark: F feature
  > # bookmark: F feature2
  > EOF
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  E=3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd
  F=d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e
  G=68ebed8873951fb1c0148207109b41e5af281a42badd0d6d33611db709b15234

  $ wait_for_bookmark_service_update repo main 3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd
  $ truncate -s 0 "$TESTTMP/bookmark-service.json"
  $ bookmark_service_client get --repo-name repo main
  3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd
Check with non-existent bookmark
  $ bookmark_service_client get --repo-name repo other
  bookmark not found
Test list feature
  $ bookmark_service_client list -R repo
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  | feature2 | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  | main     | 3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  | stable   | f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658 | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  $ bookmark_service_client list -R repo -l 1
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  $ bookmark_service_client list -R repo --after feature
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature2 | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  | main     | 3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  | stable   | f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658 | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  $ bookmark_service_client list -R repo --prefix feat
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  | feature2 | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+
  $ bookmark_service_client list -R repo --prefix feat -l 1
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+

Test list command with different scope values and verify they work
  $ bookmark_service_client --warmer-requirement hg-only list -R repo -l 1
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+

  $ bookmark_service_client --warmer-requirement git-only list -R repo -l 1
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+

  $ bookmark_service_client --warmer-requirement all-kinds list -R repo -l 1
  +----------+------------------------------------------------------------------+--------------+
  | Bookmark | Hash                                                             | Kind         |
  +----------+------------------------------------------------------------------+--------------+
  | feature  | d82958b6022e8fbd5036f8134ca6a4c9650154fce8e57e4175de90608c3d477e | pull_default |
  +----------+------------------------------------------------------------------+--------------+


Test default scope is HG_ONLY for backward compatibility
  $ bookmark_service_client get -R repo main
  3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd
  $ cat "$TESTTMP/bookmark-service.json" | jq -r 'select(.normal.method=="get" and .normal.bookmark=="main") | .normal.derived_data_set' | tail -1
  Some(DerivedDataSet::HG_ONLY)

Test invalid scope error handling
  $ bookmark_service_client --warmer-requirement invalid get -R repo main 2>&1 | grep -i "invalid"
  error: invalid value 'invalid' for '--warmer-requirement <WARMER_REQUIREMENT>'

Verify list_bookmarks also sends correct scope values
  $ bookmark_service_client --warmer-requirement hg-only list -R repo -l 1 > /dev/null
  $ cat "$TESTTMP/bookmark-service.json" | jq -r 'select(.normal.method=="list_bookmarks") | .normal.derived_data_set' | tail -1
  Some(DerivedDataSet::HG_ONLY)

  $ bookmark_service_client --warmer-requirement git-only list -R repo -l 1 > /dev/null
  $ cat "$TESTTMP/bookmark-service.json" | jq -r 'select(.normal.method=="list_bookmarks") | .normal.derived_data_set' | tail -1
  Some(DerivedDataSet::GIT_ONLY)

  $ bookmark_service_client --warmer-requirement all-kinds list -R repo -l 1 > /dev/null
  $ cat "$TESTTMP/bookmark-service.json" | jq -r 'select(.normal.method=="list_bookmarks") | .normal.derived_data_set' | tail -1
  Some(DerivedDataSet::ALL_KINDS)

  $ cat "$TESTTMP/bookmark-service.json" | summarize_scuba_json "Request complete" \
  >     .normal.method .normal.status .normal.repo .normal.bookmark .normal.error \
  >     .int.completion_time_us .int.poll_count .int.BlobGets  | jq -c
  {"BlobGets":0,"bookmark":"main","completion_time_us":\d+,"error":"","method":"get","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"bookmark":"other","completion_time_us":\d+,"error":"","method":"get","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"bookmark":"main","completion_time_us":\d+,"error":"","method":"get","poll_count":1,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
  {"BlobGets":0,"completion_time_us":\d+,"error":"","method":"list_bookmarks","poll_count":\d+,"repo":"repo","status":"SUCCESS"} (re)
