# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

  $ setup_common_config
  $ cat >> "$TESTTMP/mononoke-config/repo_definitions/repo/server.toml" <<EOF
  > preset="test"
  > EOF
  $ cat > "$TESTTMP/mononoke-config/common/presets.toml" <<EOF
  > [test]
  > phabricator_callsign="BEFORE"
  > EOF

Render the config; the loaded config is the base that candidates are compared
with:
  $ mononoke_admin_with --deployment test config show --repo repo > /dev/null 2>&1

A preset field the repo takes from the preset:
  $ mononoke_admin_with --deployment test config change-impact --preset test --field phabricator_callsign
  COMMAND: changing phabricator_callsign in preset test changes 1 repos
    changed repo

The same field as a bracketed key, and a path that does not parse:
  $ mononoke_admin_with --deployment test config change-impact --preset test --field '["phabricator_callsign"]'
  COMMAND: changing phabricator_callsign in preset test changes 1 repos
    changed repo
  $ mononoke_admin_with --deployment test config change-impact --preset test --field 'a..b' 2>&1 | grep -o "Invalid config path a..b: a name is missing"
  Invalid config path a..b: a name is missing

A field the repo sets itself:
  $ mononoke_admin_with --deployment test config change-impact --preset test --field storage_config
  COMMAND: changing storage_config in preset test changes 0 repos
  Not changed, set by spec: 1 repos
    repo

Without a candidate or a preset to plan, there is nothing to compare:
  $ mononoke_admin_with --deployment test config change-impact 2>&1 | grep "^Error"
  Error: config change-impact compares the loaded config with a candidate: give --candidate-local-configerator-path or --candidate-config, or plan a change with --preset and --field

Render a change to the preset as the candidate, keeping the loaded config as
it was:
  $ sed -i 's/BEFORE/AFTER/' "$TESTTMP/mononoke-config/common/presets.toml"
  $ render_mononoke_config
  $ CANDIDATE="$TESTTMP/candidate"
  $ cp -r "$LOCAL_CONFIGERATOR_PATH" "$CANDIDATE"
  $ sed -i 's/AFTER/BEFORE/' "$TESTTMP/mononoke-config/common/presets.toml"
  $ mononoke_admin_with --deployment test config change-impact --candidate-local-configerator-path "$CANDIDATE" --diff
  COMMAND: 1 of 1 repos change, 0 added, 0 removed
    changed repo
      repo_config.phabricator_callsign: "BEFORE" -> "AFTER"
  Changed preset test: 0 repos use it but do not change

No change:
  $ mononoke_admin_with --deployment test config change-impact --candidate-local-configerator-path "$LOCAL_CONFIGERATOR_PATH"
  COMMAND: 0 of 1 repos change, 0 added, 0 removed

As another service sees it:
  $ mononoke_admin_with --deployment test --service GIT_SERVER config change-impact --candidate-local-configerator-path "$CANDIDATE"
  GIT_SERVER: 1 of 1 repos change, 0 added, 0 removed
    changed repo
  Changed preset test: 0 repos use it but do not change

A repo's storage config and manifest entry are part of its config. Compare
edited copies of the deployment with the unedited one:
  $ DEPLOYMENTS="$LOCAL_CONFIGERATOR_PATH/scm/mononoke/deployments"
  $ CANDIDATES="$CANDIDATE/scm/mononoke/deployments"
  $ for name in storage deleted entry added common; do cp "$DEPLOYMENTS/test" "$DEPLOYMENTS/$name"; done
  $ jq '.storage.blobstore.metadata = {"local": {"local_db_path": "/elsewhere"}}' \
  >   "$DEPLOYMENTS/test" > "$CANDIDATES/storage"
  $ mononoke_admin_with --deployment storage config change-impact --candidate-local-configerator-path "$CANDIDATE" --diff
  COMMAND: 1 of 1 repos change, 0 added, 0 removed
    changed repo
      storage.metadata.local.local_db_path: "$TESTTMP/monsql" -> "/elsewhere"
  $ jq '.repos[0].is_deep_sharded = true' "$DEPLOYMENTS/test" > "$CANDIDATES/entry"
  $ mononoke_admin_with --deployment entry config change-impact --candidate-local-configerator-path "$CANDIDATE" --diff
  COMMAND: 1 of 1 repos change, 0 added, 0 removed
    changed repo
      entry.is_deep_sharded: false -> true
  $ for name in disabled reenabled explicit; do cp "$DEPLOYMENTS/test" "$DEPLOYMENTS/$name"; done
  $ jq '.repos[0].enabled = false' "$DEPLOYMENTS/test" > "$CANDIDATES/disabled"
  $ mononoke_admin_with --deployment disabled config change-impact --candidate-local-configerator-path "$CANDIDATE" --diff
  COMMAND: 1 of 1 repos change, 0 added, 0 removed
    changed repo
      entry.enabled: true -> false
  $ cp "$CANDIDATES/disabled" "$DEPLOYMENTS/reenabled"
  $ cp "$DEPLOYMENTS/test" "$CANDIDATES/reenabled"
  $ mononoke_admin_with --deployment reenabled config change-impact --candidate-local-configerator-path "$CANDIDATE" --diff
  COMMAND: 1 of 1 repos change, 0 added, 0 removed
    changed repo
      entry.enabled: false -> true
  $ jq '.repos[0].enabled = true' "$DEPLOYMENTS/test" > "$CANDIDATES/explicit"
  $ mononoke_admin_with --deployment explicit config change-impact --candidate-local-configerator-path "$CANDIDATE"
  COMMAND: 0 of 1 repos change, 0 added, 0 removed
  $ jq '.common.loadlimiter_category = "elsewhere"' "$DEPLOYMENTS/test" > "$CANDIDATES/common"
  $ mononoke_admin_with --deployment common config change-impact --candidate-local-configerator-path "$CANDIDATE"
  COMMAND: 0 of 1 repos change, 0 added, 0 removed
  Common config of every repo changes: loadlimiter_category

A service's legacy tier can only change with a restart:
  $ cp "$DEPLOYMENTS/test" "$DEPLOYMENTS/tier"
  $ jq '.services |= with_entries(.value.legacy_tier = "elsewhere")' "$DEPLOYMENTS/test" > "$CANDIDATES/tier"
  $ mononoke_admin_with --deployment tier config change-impact --candidate-local-configerator-path "$CANDIDATE"
  COMMAND: 0 of 1 repos change, 0 added, 0 removed
  warning: the legacy tier changes from test to elsewhere: running jobs refuse this and keep the old config until they restart

Deleting the storage config a repo uses, or adding a repo whose spec cannot be
resolved, fails. (The redaction config moves to a copy of the storage config,
since a deployment whose common config names a missing one does not load.)
  $ jq '.storage.other = .storage.blobstore | del(.storage.blobstore)
  >   | .common.redaction_config |= with_entries(if .value == "blobstore" then .value = "other" else . end)' \
  >   "$DEPLOYMENTS/test" > "$CANDIDATES/deleted"
  $ mononoke_admin_with --deployment deleted config change-impact --candidate-local-configerator-path "$CANDIDATE" 2> stderr
  COMMAND: 0 of 1 repos change, 0 added, 0 removed
  Common config of every repo changes: redaction_config
    failed repo: resolving the config of repo: invalid config options: Storage "blobstore" not defined
  [1]
  $ grep "^Error" stderr
  Error: 1 repos failed to resolve
  $ jq '.repos += [.repos[0] | .repo_name = "added" | .repo_id = 99 | .spec_path = "test/missing"]' \
  >   "$DEPLOYMENTS/test" > "$CANDIDATES/added"
  $ mononoke_admin_with --deployment added config change-impact --candidate-local-configerator-path "$CANDIDATE" 2> stderr
  COMMAND: 0 of 1 repos change, 1 added, 0 removed
    added added
    failed added: loading the spec of added: failed to open $TESTTMP/candidate/scm/mononoke/repos/test/missing: No such file or directory (os error 2)
  [1]
  $ grep "^Error" stderr
  Error: 1 repos failed to resolve

Planning lists the repos it can't resolve, and fails, after those it can:
  $ jq '.repos += [.repos[0] | .repo_name = "broken" | .repo_id = 98 | .spec_path = "test/missing"]' \
  >   "$DEPLOYMENTS/test" > "$DEPLOYMENTS/broken"
  $ mononoke_admin_with --deployment broken config change-impact --preset test --field phabricator_callsign 2> stderr
  COMMAND: changing phabricator_callsign in preset test changes 1 repos
    changed repo
    failed broken: loading the spec of broken: failed to open * (glob)
  [1]
  $ grep "^Error" stderr
  Error: 1 repos failed to resolve

Change impact needs a deployment:
  $ mononoke_admin config change-impact --preset test --field storage_config 2> stderr
  [1]
  $ grep "^Error" stderr
  Error: config change-impact needs a deployment, such as --deployment prod

A deployment loaded with --config is a base too, and --candidate-config names a
candidate laid out the same way:
  $ LOCAL="$TESTTMP/local_deployment"
  $ mkdir -p "$LOCAL/deployments"
  $ cp "$DEPLOYMENTS/test" "$LOCAL/deployments/test.json"
  $ cp -r "$LOCAL_CONFIGERATOR_PATH/scm/mononoke/repos" "$LOCAL/repos"
  $ find "$LOCAL/repos" -type f -exec mv {} {}.json \;
  $ mononoke_admin_with --config "$LOCAL" --deployment test config change-impact --preset test --field phabricator_callsign
  COMMAND: changing phabricator_callsign in preset test changes 1 repos
    changed repo
  $ cp -r "$LOCAL" "$TESTTMP/local_candidate"
  $ jq '.common.loadlimiter_category = "elsewhere"' "$LOCAL/deployments/test.json" \
  >   > "$TESTTMP/local_candidate/deployments/test.json"
  $ mononoke_admin_with --config "$LOCAL" --deployment test config change-impact --candidate-config "$TESTTMP/local_candidate"
  COMMAND: 0 of 1 repos change, 0 added, 0 removed
  Common config of every repo changes: loadlimiter_category

A union set to another variant shows its variant line, then the values that
come and go:
  $ SERVER_TOML="$TESTTMP/mononoke-config/repos/repo/server.toml"
  $ cp "$SERVER_TOML" "$TESTTMP/server.toml.orig"
  $ sed -i 's/^local_zelos_port = .*/zelos_tier = "tier"/' "$SERVER_TOML"
  $ render_mononoke_config
  $ rm -rf "$CANDIDATE"
  $ cp -r "$LOCAL_CONFIGERATOR_PATH" "$CANDIDATE"
  $ mv "$TESTTMP/server.toml.orig" "$SERVER_TOML"
  $ mononoke_admin_with --deployment test config change-impact --candidate-local-configerator-path "$CANDIDATE" --diff
  COMMAND: 1 of 1 repos change, 0 added, 0 removed
    changed repo
      repo_config.zelos_config: local_zelos_port -> zelos_tier
      repo_config.zelos_config.local_zelos_port: \d+ -> \(unset\) (re)
      repo_config.zelos_config.zelos_tier: (unset) -> "tier"
