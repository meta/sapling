# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ cat >> "$ACL_FILE" << ACLS
  > {
  >   "repos": {
  >     "default": {
  >       "actions": {
  >         "read": [
  >           "$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA",
  >           "$CLIENT1_ID_TYPE:$CLIENT1_ID_DATA"
  >         ],
  >         "write": ["$CLIENT0_ID_TYPE:$CLIENT0_ID_DATA"]
  >       }
  >     }
  >   },
  >   "repo_regions": {
  >     "restricted_acl": {
  >       "actions": {
  >         "read": ["$CLIENT1_ID_TYPE:$CLIENT1_ID_DATA"]
  >       }
  >     }
  >   }
  > }
  > ACLS

  $ setup_common_config
  $ cd "$TESTTMP/mononoke-config"
  $ cat >> repos/repo/server.toml << EOF
  > [restricted_paths_config]
  > path_restriction_metadata = { "restricted" = { repo_region_acl = "REPO_REGION:restricted_acl" } }
  > [restricted_paths_config.manifest_id_store_config]
  > use_manifest_id_cache = false
  > cache_update_interval_ms = 1000
  > EOF

  $ testtool_drawdag -R repo --derive-all --no-default-files << EOF
  > A
  > # bookmark: A main
  > # modify: A restricted/file.txt secret
  > # modify: A unrestricted/file.txt public
  > EOF
  A=* (glob)

  $ start_and_wait_for_scs_server
  $ export SCSC_ADMIN_ENABLED=1

The access command surfaces the permission request group (group name only) for a restricted path
  $ scsc_as client1 restricted-paths access -R repo -i $A --check-permissions -p restricted/file.txt | grep -o 'request group: restricted_acl'
  request group: restricted_acl

The access command includes the permission request group in JSON output
  $ scsc_as client1 restricted-paths access -R repo -i $A --check-permissions -p restricted/file.txt --json | grep -o '"permission_request_group":"restricted_acl"'
  "permission_request_group":"restricted_acl"

The changes command surfaces the permission request group for changed restricted paths
  $ scsc_as client1 restricted-paths changes -R repo -i $A | grep -o 'request group: restricted_acl'
  request group: restricted_acl

The changes command includes the permission request group in JSON output
  $ scsc_as client1 restricted-paths changes -R repo -i $A --json | grep -o '"permission_request_group":"restricted_acl"'
  "permission_request_group":"restricted_acl"
