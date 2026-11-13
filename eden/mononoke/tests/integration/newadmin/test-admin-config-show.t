# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

  $ setup_common_config

A repo's resolved config, one line per value, as this tier loads it:
  $ mononoke_admin config show --repo repo 2> stderr | grep -E '^(repo_name|repo_id|readonly|tiers\[0\]|repo_config\.storage_config|repo_config\.derived_data_config\.enabled_config_name) '
  readonly = false
  repo_id = 0
  repo_name = "repo"
  tiers[0] = "test"
  repo_config.derived_data_config.enabled_config_name = "default"
  repo_config.storage_config = "blobstore"
  $ cat stderr
  Resolved config of repo on tier test

The same config as JSON:
  $ mononoke_admin config show --repo repo --json 2> /dev/null | jq -c '{repo_name, repo_id, readonly, storage_config: .repo_config.storage_config, enabled_derived_data: .repo_config.derived_data_config.enabled_config_name}'
  {"repo_name":"repo","repo_id":0,"readonly":false,"storage_config":"blobstore","enabled_derived_data":"default"}

A union's line names the variant it holds, before the variant's own values:
  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" <<EOF
  > [metadata_cache_config.wbc_update_mode.polling]
  > EOF
  $ mononoke_admin config show --repo repo 2> /dev/null | grep -E "^repo_config\.(metadata_cache_config|zelos_config)"
  repo_config.metadata_cache_config.wbc_update_mode = polling
  repo_config.zelos_config = local_zelos_port
  repo_config.zelos_config.local_zelos_port = \d+ (re)

The storage config the repo names follows its config, under `storage`:

  $ mononoke_admin config show --repo repo 2> /dev/null | grep -E "^storage\.metadata"
  storage.metadata = local
  storage.metadata.local.local_db_path = .* (re)

  $ mononoke_admin config show --repo repo --json 2> /dev/null | jq -c '.storage.metadata | keys'
  ["local"]

A config naming a storage config that is not defined is still shown, without
the storage config, and the command fails:
  $ CONFIG="$TESTTMP/mononoke-config/repos/repo/server.toml"
  $ cp "$CONFIG" "$TESTTMP/server.toml.good"
  $ sed -i 's/^storage_config *=.*/storage_config = "missing"/' "$CONFIG"
  $ mononoke_admin config show --repo repo > out 2> err
  [1]
  $ grep -E "^(repo_name|repo_config\.storage_config|# storage|storage\.)" out
  repo_name = "repo"
  repo_config.storage_config = "missing"
  # storage: invalid config options: Storage "missing" not defined
  $ grep "^Error" err
  Error: invalid config options: Storage "missing" not defined
  $ mononoke_admin config show --repo repo --json 2> /dev/null | jq -c '{storage_config: .repo_config.storage_config, storage}'
  {"storage_config":"missing","storage":null}
  $ cp "$TESTTMP/server.toml.good" "$CONFIG"

A repo that is not in the tier:
  $ mononoke_admin config show --repo unknown
  Error: repo unknown is not in the test tier manifest
  [1]
