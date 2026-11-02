# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ REPONAME="test/repo"
  $ setup_common_config
  $ setup_configerator_configs
  $ cd "$TESTTMP"
  $ testtool_drawdag -R test/repo <<'EOF'
  > A-B-C
  > # bookmark: C main
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

Query bookmark
  $ scsc info -R test/repo -B main -S bonsai --bookmark-info
  Warm value: e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  Fresh value: e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  Last update: 20* (glob)

Move bookmark
  $ testtool_drawdag -R test/repo <<EOF
  > C-D-E
  > # exists: C $C
  > # bookmark: D main
  > EOF
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  E=3a2426d009267ba6f83945ecb29f63116a21984fb62df772d3bbe0143163b8fd

Sleep for 10 seconds so the warm bookmark cache definitely catches up
  $ sleep 10

Query bookmark after move
  $ scsc info -R test/repo -B main -S bonsai --bookmark-info
  Warm value: f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  Fresh value: f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  Last update: 20* (glob)
