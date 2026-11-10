# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# A server whose repos are assigned by a local shard file, as ShardManager
# assigns them in production: a deep-sharded repo is served only while it is
# assigned, and a shallow one always.

  $ . "${TEST_FIXTURES}/library.sh"

  $ setup_common_config
  $ setup_configerator_configs
  $ REPOID=1 DEEP_SHARDED_SERVICES="EDEN_API" setup_mononoke_repo_config "sharded_repo"
  $ quiet testtool_drawdag -R repo <<EOF
  > A
  > # bookmark: A master_bookmark
  > EOF
  $ quiet testtool_drawdag -R sharded_repo <<EOF
  > A
  > # bookmark: A master_bookmark
  > EOF

  $ capabilities() {
  >   sslcurlas client0 -s -o /dev/null -w 'HTTP %{http_code}\n' "https://localhost:$MONONOKE_SOCKET/edenapi/$1/capabilities"
  > }

Start with nothing assigned. The deep-sharded repo is configured here but not
loaded, so requests for it are retriable:
  $ SHARDS="$TESTTMP/shards"
  $ set_local_shards "$SHARDS"
  $ start_and_wait_for_mononoke_server --local-shardmanager-path "$SHARDS"
  $ wait_for_local_shards "$SHARDS"
  $ capabilities sharded_repo
  HTTP 503
  $ capabilities repo
  HTTP 200

Assigning it loads it:
  $ set_local_shards "$SHARDS" sharded_repo
  $ wait_for_local_shards "$SHARDS"
  $ cat "$SHARDS.active"
  sharded_repo
  $ capabilities sharded_repo
  HTTP 200

Dropping it unloads it:
  $ set_local_shards "$SHARDS"
  $ wait_for_local_shards "$SHARDS"
  $ capabilities sharded_repo
  HTTP 503

Dropping a shallow repo keeps it served:
  $ set_local_shards "$SHARDS" repo
  $ wait_for_local_shards "$SHARDS"
  $ set_local_shards "$SHARDS"
  $ wait_for_local_shards "$SHARDS"
  $ capabilities repo
  HTTP 200

A shard that fails to set up is listed with its error, and the others still
apply:
  $ set_local_shards "$SHARDS" sharded_repo unknown_repo
  $ wait_for_local_shard_error "$SHARDS" unknown_repo
  $ cat "$SHARDS.active"
  sharded_repo
  $ cut -d: -f1-2 "$SHARDS.errors"
  unknown_repo: Failure in setting up repo unknown_repo in Mononoke service
