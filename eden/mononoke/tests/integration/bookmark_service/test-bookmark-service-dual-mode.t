# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for SCS WBC dual mode.
# Validates the unconditional per-identity-scheme routing:
# - HG repos use the remote Bookmark Service (and fail when it's killed)
# - Git repos use local WBC (and continue working when Bookmark Service is killed)

setup
  $ . "${TEST_FIXTURES}/library.sh"

Setup HG repo (default commit identity scheme)
  $ REPONAME="hg_repo" setup_common_config
  $ testtool_drawdag -R hg_repo <<'EOF'
  > A-B-C
  > # bookmark: C main
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Setup Git repo (commit identity scheme = GIT)
  $ REPOID=1 REPONAME="git_repo" COMMIT_IDENTITY_SCHEME=3 setup_common_config
  $ testtool_drawdag -R git_repo <<'EOF'
  > D-E-F
  > # bookmark: F main
  > EOF
  D=* (glob)
  E=* (glob)
  F=* (glob)

Start bookmark service (serves both repos)
  $ start_and_wait_for_bookmark_service

Start SCS with remote bookmark cache pointing to bookmark service
  $ ENABLE_BOOKMARK_CACHE=1 start_and_wait_for_scs_server \
  >   --use-remote-bookmark-cache true \
  >   --remote-bookmark-cache-host-port "localhost:$BOOKMARK_SERVICE_PORT"

Verify HG repo can lookup bookmarks (through remote Bookmark Service)
  $ scsc lookup -R hg_repo -B main -S bonsai
  e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Verify Git repo can lookup bookmarks (through local WBC)
  $ scsc lookup -R git_repo -B main -S bonsai
  * (glob)

Kill the bookmark service to prove routing difference
  $ kill -9 $BOOKMARK_SERVICE_PID
  $ sleep 2
  * Killed * (glob)

HG repo should fail (it was using the remote Bookmark Service which is now dead)
  $ scsc lookup -R hg_repo -B main -S bonsai
  *: error: SourceControlService::repo_resolve_bookmark failed with internal_error(InternalError): InternalError: BookmarkService::get failed with ThriftError* (glob)
  [1]

Git repo should still work (it uses local WBC, not the remote Bookmark Service)
  $ scsc lookup -R git_repo -B main -S bonsai
  * (glob)
