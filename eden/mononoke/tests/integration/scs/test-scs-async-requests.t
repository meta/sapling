# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ REPONAME="test/repo"
  $ setup_common_config
  $ testtool_drawdag --repo-name "$REPONAME" << EOF
  > A
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  $ start_and_wait_for_scs_server

empty list
  $ mononoke_admin --log-level OFF async-requests list --repo-name "$REPONAME"
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+
  | Request id | Method | Status | Target | Source name (sync_changeset) | Source Changeset (sync_changeset) | Created at | Ready at | Duration |
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+

an entry with a bad args_blobstore_key
  $ async_worker_enqueue 0 stash_test-60-repos megarepo_add_sync_target async.svc.unknown.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e
  $ mononoke_admin --log-level OFF async-requests list --repo-name "$REPONAME"
  Error: deserializing
  
  Caused by:
      0: async.svc.unknown.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e is not a blobstore key for AsynchronousRequestParams
      1: async.svc.unknown.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e is not a blobstore key for AsynchronousRequestParams
      2: async.svc.unknown.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e is not a blobstore key for AsynchronousRequestParams, skipping
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+
  | Request id | Method | Status | Target | Source name (sync_changeset) | Source Changeset (sync_changeset) | Created at | Ready at | Duration |
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+

  $ async_requests_clear_queue

an entry with a valid args_blobstore_key that is missing from the store
  $ async_worker_enqueue 0 stash_test-60-repos megarepo_add_sync_target async.svc.AsynchronousRequestParams.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e
  $ mononoke_admin --log-level OFF async-requests list --repo-name "$REPONAME"
  Error: deserializing
  
  Caused by:
      0: Missing blob: async.svc.AsynchronousRequestParams.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e
      1: Missing blob: async.svc.AsynchronousRequestParams.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e
      2: Missing blob: async.svc.AsynchronousRequestParams.blake2.b502b0b3b99fcb23e82687e5c88c5f2bd0647c4efb90450c4db209007d65d20e, skipping
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+
  | Request id | Method | Status | Target | Source name (sync_changeset) | Source Changeset (sync_changeset) | Created at | Ready at | Duration |
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+
  +------------+--------+--------+--------+------------------------------+-----------------------------------+------------+----------+----------+

  $ async_requests_clear_queue

a valid, new entry
  $ mononoke_admin --log-level OFF async-requests submit --repo-name "$REPONAME" -m megarepo_add_sync_target -p <(cat <<EOF
  > {
  >   "config_with_new_target": {
  >     "target": {
  >       "repo": {
  >         "name": "test/repo"
  >       },
  >       "bookmark": "releases-oculus-12.0-adhoc-prebuilts-repo-v1"
  >     }
  >   }
  > }
  > EOF
  > )
  Submitted with token: 3
  $ mononoke_admin --log-level OFF async-requests list --repo-name "$REPONAME"
  +------------+--------------------------+--------+------------------------------------------------------------------------------+------------------------------+-----------------------------------+* (glob)
  | Request id | Method                   | Status | Target                                                                       | Source name (sync_changeset) | Source Changeset (sync_changeset) |* (glob)
  +------------+--------------------------+--------+------------------------------------------------------------------------------+------------------------------+-----------------------------------+* (glob)
  | 3          | megarepo_add_sync_target | new    | repo_name: test/repo, bookmark: releases-oculus-12.0-adhoc-prebuilts-repo-v1 |                              |                                   | * | Not finished | Not finished | (glob)
  +------------+--------------------------+--------+------------------------------------------------------------------------------+------------------------------+-----------------------------------+* (glob)

  $ mononoke_admin --log-level OFF async-requests submit --repo-name "$REPONAME" -m megarepo_add_sync_target -p <(cat <<EOF
  > {
  >   "config_with_new_target": {
  >     "target": {
  >       "repo": {
  >         "name": "test/repo"
  >       },
  >       "bookmark": "releases-oculus-12.0-adhoc-prebuilts-repo-v1"
  >     }
  >   }
  > }
  > EOF
  > )
  Submitted with token: 4
  $ mononoke_admin --log-level OFF async-requests submit --repo-name "$REPONAME" -m megarepo_add_sync_target -p <(cat <<EOF
  > {
  >   "config_with_new_target": {
  >     "target": {
  >       "repo": {
  >         "name": "test/repo"
  >       },
  >       "bookmark": "releases-oculus-12.0-adhoc-prebuilts-repo-v1"
  >     }
  >   }
  > }
  > EOF
  > )
  Submitted with token: 5
  $ mononoke_admin --log-level OFF async-requests list --repo-name "$REPONAME"
  +------------+--------------------------+--------+------------------------------------------------------------------------------+------------------------------+-----------------------------------+* (glob)
  | Request id | Method                   | Status | Target                                                                       | Source name (sync_changeset) | Source Changeset (sync_changeset) |* (glob)
  +------------+--------------------------+--------+------------------------------------------------------------------------------+------------------------------+-----------------------------------+* (glob)
  | 3          | megarepo_add_sync_target | new    | repo_name: test/repo, bookmark: releases-oculus-12.0-adhoc-prebuilts-repo-v1 |                              |                                   | * | Not finished | Not finished | (glob)
  | 4          | megarepo_add_sync_target | new    | repo_name: test/repo, bookmark: releases-oculus-12.0-adhoc-prebuilts-repo-v1 |                              |                                   | * | Not finished | Not finished | (glob)
  | 5          | megarepo_add_sync_target | new    | repo_name: test/repo, bookmark: releases-oculus-12.0-adhoc-prebuilts-repo-v1 |                              |                                   | * | Not finished | Not finished | (glob)
  +------------+--------------------------+--------+------------------------------------------------------------------------------+------------------------------+-----------------------------------+* (glob)
  $ async_requests_clear_queue
