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
  > A-B-C
  > # bookmark: A main
  > # modify: A restricted/file.txt secret
  > # modify: A unrestricted/file.txt public
  > # modify: B restricted/file.txt secret2
  > # modify: C restricted/file.txt secret
  > # modify: C unrestricted/other.txt public2
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

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
  $ scsc_as client1 restricted-paths changes -R repo -i $A --json | jq -r '.compact.policies[].permission_request_group_identity'
  REPO_REGION:restricted_acl

An explicit range uses the net endpoint difference, excluding changes that were reverted.
  $ scsc_as client0 restricted-paths changes -R repo -i $A -i $C --json | jq -c '{are_restricted, has_access, changed_paths}'
  {"are_restricted":"none","has_access":"all","changed_paths":["unrestricted/other.txt"]}

The default first-parent comparison includes the reverted file's change in the tip.
  $ scsc_as client0 restricted-paths changes -R repo -i $C --json | jq -c '{are_restricted, has_access, changed_paths, authorized_paths}'
  {"are_restricted":"some","has_access":"some","changed_paths":["restricted/file.txt","unrestricted/other.txt"],"authorized_paths":["unrestricted/other.txt"]}

More than two commit arguments are rejected before making the request.
  $ scsc_as client0 restricted-paths changes -R repo -i $A -i $B -i $C 2>&1 | grep -o 'expected one target commit or a base and target commit'
  expected one target commit or a base and target commit
