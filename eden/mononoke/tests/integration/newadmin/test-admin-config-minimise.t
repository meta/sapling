# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

  $ setup_common_config

Proposed presets: one that the repo's config fully contains, and one that sets
a field the repo does not have:
  $ cat > "$TESTTMP/proposed.json" <<EOF
  > {
  >   "same": {"storage_config": "blobstore"},
  >   "extra": {"storage_config": "blobstore", "phabricator_callsign": "PRESET"}
  > }
  > EOF

The spec names the preset and drops what it shares with it:
  $ mononoke_admin_with --deployment test config minimise --repo repo --presets "$TESTTMP/proposed.json" --preset same --json | jq -c '{preset, storage_config: .spec.repo_config.storage_config, spec_preset: .spec.preset, smaller: (.bytes_after < .bytes_before)}'
  {"preset":"same","storage_config":null,"spec_preset":"same","smaller":true}

A preset that would add a field the repo does not have cannot be used:
  $ mononoke_admin_with --deployment test config minimise --repo repo --presets "$TESTTMP/proposed.json" --preset extra --json | jq -c '{spec, problems}'
  {"spec":null,"problems":{"extra":["phabricator_callsign (tier test)"]}}

A spec in a tier that no service of the deployment uses is minimised, and that
tier is named, since its jobs can't resolve a preset:
  $ SPECS="$LOCAL_CONFIGERATOR_PATH/scm/mononoke/repos/test"
  $ DEPLOYMENTS="$LOCAL_CONFIGERATOR_PATH/scm/mononoke/deployments"
  $ jq '.tiers += ["readonly"]' "$SPECS/repo" > "$SPECS/readonly_repo"
  $ jq '.repos[0].spec_path = "test/readonly_repo"' "$DEPLOYMENTS/test" > "$DEPLOYMENTS/readonly"
  $ mononoke_admin_with --deployment readonly config minimise --repo repo --presets "$TESTTMP/proposed.json" --preset same
  repo: preset same, * -> * bytes (in tiers no service uses: readonly) (glob)
  Minimised 1 specs: * -> * bytes (glob)

Without --preset, the smallest spec wins:
  $ mononoke_admin_with --deployment test config minimise --repo repo --presets "$TESTTMP/proposed.json" --json | jq -c '{repo, preset}'
  {"repo":"repo","preset":"same"}

Without --presets, the deployment's own presets:
  $ cat > "$TESTTMP/mononoke-config/common/presets.toml" <<EOF
  > [deployed]
  > storage_config="blobstore"
  > EOF
  $ mononoke_admin_with --deployment test config minimise --all
  repo: preset deployed, * -> * bytes (glob)
  Minimised 1 specs: * -> * bytes (glob)

Without presets there is nothing to minimise against:
  $ rm "$TESTTMP/mononoke-config/common/presets.toml"
  $ mononoke_admin_with --deployment test config minimise --all
  Error: there are no presets to minimise against: the deployment has none, so pass --presets
  [1]

Minimise needs a deployment:
  $ mononoke_admin config minimise --repo repo 2> stderr
  [1]
  $ grep "^Error" stderr
  Error: config minimise needs a deployment, such as --deployment prod
