# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

  $ INFINITEPUSH_ALLOW_WRITES=true setup_common_config
  $ setup_configerator_configs
  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" <<EOF
  > [[bookmarks]]
  > name="trunk"
  > only_fast_forward=true
  > EOF

  $ testtool_drawdag --repo-name repo <<EOF
  > C
  > |
  > B   E
  > |   |
  > A   D
  >     |
  >     A
  > # bookmark: C trunk
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=fa8ba037ceed6e3f11f3bd0d21a866ca4c7a8c721ff13ca7c0b3442e1e4cbb16
  E=2b9b61bcf926e1e354ecb23c529868b1368af2e315b7b8ad720795184764020c

  $ start_and_wait_for_land_service
  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" <<EOF
  > [pushrebase.remote_mode]
  > remote_land_service = { host_port = "$(land_service_address)" }
  > EOF
  $ start_and_wait_for_scs_server
  $ export SCSC_WRITES_ENABLED=true

  $ scsc land-stack -R repo --name trunk -i $E -i $A
  In 0 retries across distance 2
  trunk updated to: 2576855b2ced4f17d5cf3daa80dd1b9d4b35ddce
  2b9b61bcf926e1e354ecb23c529868b1368af2e315b7b8ad720795184764020c => 2576855b2ced4f17d5cf3daa80dd1b9d4b35ddce
  fa8ba037ceed6e3f11f3bd0d21a866ca4c7a8c721ff13ca7c0b3442e1e4cbb16 => 74dbcd84493ad579ee26bb326c4272983098f69c

  $ scsc lookup -R repo -B trunk -S bonsai
  3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd
