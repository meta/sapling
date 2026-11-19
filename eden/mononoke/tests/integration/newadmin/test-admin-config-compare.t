# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

  $ setup_common_config

The harness renders the test tier as a deployment too, in which every service
sees the tier:
  $ DEPLOYMENTS="$LOCAL_CONFIGERATOR_PATH/scm/mononoke/deployments"

Every service of the deployment sees what a job running with its legacy tier sees:
  $ mononoke_admin_with --deployment test config compare | grep -c "(test): same$"
  25
  $ mononoke_admin_with --deployment test config compare SCS_SERVER GIT_SERVER
  SCS_SERVER (test): same
  GIT_SERVER (test): same

A deployment that stops serving the repo from SCS and changes a common config
field differs from the tier:
  $ jq '.repos[0].services = {"3": false} | .common.loadlimiter_category = "elsewhere"' \
  >   "$DEPLOYMENTS/test" > "$DEPLOYMENTS/changed"
  $ mononoke_admin_with --deployment changed config compare SCS_SERVER GIT_SERVER
  SCS_SERVER (test): differs
    repos only in the tier: repo
    common config differs in: loadlimiter_category
  GIT_SERVER (test): differs
    common config differs in: loadlimiter_category
  Error: 2 of 2 services differ from their tiers
  [1]

Disabling the repo in the deployment's entry alone differs from the tier, and an
explicit enabled = true is the same as leaving it unset:
  $ jq '.repos[0].enabled = false' "$DEPLOYMENTS/test" > "$DEPLOYMENTS/disabled"
  $ mononoke_admin_with --deployment disabled config compare SCS_SERVER
  SCS_SERVER (test): differs
    repos enabled differently: repo
  Error: 1 of 1 services differ from their tiers
  [1]
  $ jq '.repos[0].enabled = true' "$DEPLOYMENTS/test" > "$DEPLOYMENTS/enabled"
  $ mononoke_admin_with --deployment enabled config compare SCS_SERVER
  SCS_SERVER (test): same

Compare needs a deployment:
  $ mononoke_admin config compare
  Error: config compare needs a deployment, such as --deployment prod
  [1]

A program loads its service's view of the deployment, the admin tool COMMAND's:
  $ mononoke_admin_with --deployment test config show --repo repo 2>&1 >/dev/null
  Resolved config of repo as COMMAND sees it (legacy tier test)
  $ mononoke_admin_with --deployment test --service GIT_SERVER config show --repo repo 2>&1 >/dev/null
  Resolved config of repo as GIT_SERVER sees it (legacy tier test)
  $ mononoke_admin_with --deployment changed --service SCS_SERVER config show --repo repo
  Error: repo repo is not served by SCS_SERVER
  [1]

The repo opens through the deployment:
  $ mononoke_admin_with --deployment test bookmarks -R repo list

A deployment can be read from a directory of .json files instead, laid out as
deployments/<name>.json and repos/<spec path>.json:
  $ LOCAL="$TESTTMP/local_deployment"
  $ mkdir -p "$LOCAL/deployments"
  $ cp "$DEPLOYMENTS/test" "$LOCAL/deployments/test.json"
  $ cp -r "$LOCAL_CONFIGERATOR_PATH/scm/mononoke/repos" "$LOCAL/repos"
  $ find "$LOCAL/repos" -type f -exec mv {} {}.json \;
  $ mononoke_admin_with --config "$LOCAL" --deployment test config show --repo repo 2>&1 >/dev/null
  Resolved config of repo as COMMAND sees it (legacy tier test)
  $ mononoke_admin_with --config "$LOCAL" --deployment test config compare --specs SCS_SERVER
  SCS_SERVER (test): same

A spec field outside the repo's config, such as readonly, is compared too:
  $ jq '.readonly = true' "$LOCAL/repos/test/repo.json" > "$TESTTMP/readonly.json"
  $ mv "$TESTTMP/readonly.json" "$LOCAL/repos/test/repo.json"
  $ mononoke_admin_with --config "$LOCAL" --deployment test config compare --specs SCS_SERVER
  SCS_SERVER (test): differs
    repos whose configs resolve differently: repo
  Error: 1 of 1 services differ from their tiers
  [1]

With --specs, each repo's config is resolved both ways and compared:
  $ SPECS="$LOCAL_CONFIGERATOR_PATH/scm/mononoke/repos/test"
  $ jq '.repo_config.hook_max_file_size = 12345' "$SPECS/repo" > "$SPECS/changed_repo"
  $ jq '.repos[0].spec_path = "test/changed_repo"' \
  >   "$DEPLOYMENTS/test" > "$DEPLOYMENTS/changed_spec"
  $ mononoke_admin_with --deployment changed_spec config compare --specs SCS_SERVER
  SCS_SERVER (test): differs
    repos with a different spec path: repo
    repos whose configs resolve differently: repo
  Error: 1 of 1 services differ from their tiers
  [1]

A service the deployment does not have, and a service without a deployment,
are refused:
  $ jq '.services = {"3": .services["3"]}' "$DEPLOYMENTS/test" > "$DEPLOYMENTS/scs_only"
  $ mononoke_admin_with --deployment scs_only --service GIT_SERVER config show --repo repo 2>&1 | grep -o "has no service GIT_SERVER"
  has no service GIT_SERVER
  $ mononoke_admin --service SCS_SERVER config show --repo repo > /dev/null 2>&1
  [2]
