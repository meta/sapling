# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for bookmark service with SCS
# This test validates that SCS can use the remote bookmark service
# by verifying that bookmark operations fail when the service is stopped.

setup
  $ . "${TEST_FIXTURES}/library.sh"

  $ REPONAME="repo" setup_common_config

  $ testtool_drawdag -R repo << EOF
  > A-B-C
  > # bookmark: C main
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Start bookmark service
  $ start_and_wait_for_bookmark_service
  $ wait_for_bookmark_service_update repo main $C

Verify the bookmark service has the bookmark
  $ bookmark_service_client get -R repo main
  e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Start SCS with remote bookmark cache pointing to bookmark service
  $ ENABLE_BOOKMARK_CACHE=1 start_and_wait_for_scs_server \
  >   --use-remote-bookmark-cache true \
  >   --remote-bookmark-cache-host-port "localhost:$BOOKMARK_SERVICE_PORT" \
  >   --scuba-log-file "$TESTTMP/scs_scuba.json"

Verify SCS can lookup bookmarks through the remote service
  $ scsc lookup -R repo -B main -S bonsai
  e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Verify SCS requested the bookmark with ALL_KINDS scope
  $ jq -r 'select(.normal.method=="get" and .normal.bookmark=="main") | .normal.derived_data_set' \
  >   "$TESTTMP/bookmark-service.json" | tail -1
  Some(DerivedDataSet::ALL_KINDS)

Stop the bookmark service to prove SCS is using it
  $ kill -9 $BOOKMARK_SERVICE_PID
  $ sleep 2
  * Killed * (glob)

Verify SCS now fails to lookup bookmarks (proving it was using the remote service)
  $ scsc lookup -R repo -B main -S bonsai
  *: error: SourceControlService::repo_resolve_bookmark failed with internal_error(InternalError): InternalError: BookmarkService::get failed with ThriftError* (glob)
  [1]
