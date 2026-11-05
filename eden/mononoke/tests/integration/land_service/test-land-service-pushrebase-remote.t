# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ enable rebase pushrebase commitcloud
  $ setconfig push.edenapi=true
  $ function stablelog() {
  >   hg log -G -T "{desc} {phase} {remotenames}" --rev '::remote/master_bookmark' | sed 's/^[ \t]*$/$/'
  > }

Setup repo
  $ SCUBA_LOGGING_PATH="$TESTTMP/scuba.json"
  $ setup_common_config
  $ cd $TESTTMP
  $ testtool_drawdag -R repo << EOF
  >   D F Z
  >   | | |
  > X C E Y
  > | |/ /
  > W B /
  >  \|/
  >   A
  > # bookmark: D master_bookmark
  > # modify: W B Bconflict
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  E=34a8be8c9dc6e46c954a330f06f792cb62a133d3f7ce9b46e70b91d358970c70
  F=7d7e98c04f7dd3f4be0916b4f0b95fa44747b065f105bd36b87252356ae0d0f5
  W=eac6b6808a810c3f3c11084d2350670fd4e0b88e42cdab06d6f9305f3d0a1a9f
  X=310545e2034569b3d179f8ba18544c7c0316114d5ca46ae2efe7429f1ad8869c
  Y=5982e8fadbb37c4213b099440e99f93840a120792e802fbdf52fee3a62730dde
  Z=6767e98e810fddcf86321e083675054d33777286dc61687db83037ee1a493365
Start mononoke
  $ start_and_wait_for_land_service
  $ cd $TESTTMP/mononoke-config
  $ cp repos/repo/server.toml repos/repo/server.toml.BACKUP
  $ cat >> repos/repo/server.toml << EOF
  > [pushrebase.remote_mode]
  > remote_land_service = { host_port = "$(land_service_address)" }
  > EOF
  $ cd $TESTTMP
  $ start_and_wait_for_mononoke_server --scuba-log-file "$SCUBA_LOGGING_PATH"
Test pushrebase
  $ hg clone -q  mono:repo client1
  $ cd client1
  $ quiet hg up $(mononoke_admin convert -R repo $F --from bonsai --to hg --derive)
  $ stablelog
  o  D public remote/master_bookmark
  │
  o  C public
  │
  o  B public
  │
  o  A public
  $
  $ quiet hg push --to master_bookmark
  $ stablelog
  @  F public remote/master_bookmark
  │
  o  E public
  │
  o  D public
  │
  o  C public
  │
  o  B public
  │
  o  A public
  $
  $ quiet hg up $(mononoke_admin convert -R repo $Y --from bonsai --to hg --derive)
  $ quiet hg push --to master_bookmark
  $ stablelog
  @  Y public remote/master_bookmark
  │
  o  F public
  │
  o  E public
  │
  o  D public
  │
  o  C public
  │
  o  B public
  │
  o  A public
  $
Try a push that fails with conflict
  $ quiet hg up $(mononoke_admin convert -R repo $W --from bonsai --to hg --derive)
  $ hg push --to master_bookmark
  pushing rev e9a541efaed1 to destination https://localhost:*/edenapi/ bookmark master_bookmark (glob)
  pushrebasing stack (20ca2a4749a4, e9a541efaed1] (1 commit) to remote bookmark master_bookmark
  abort: Server error: Conflicts while pushrebasing: [PushrebaseConflict { left: MPath("B"), right: MPath("B") }]
  [255]

If Land service is off, the push should fail.
  $ killandwait $LAND_SERVICE_PID
  $ quiet hg up $(mononoke_admin convert -R repo $Z --from bonsai --to hg --derive)
  $ hg rebase -s . -d remote/master_bookmark
  rebasing acd71d4b8eaa "Z"
  $ quiet hg push --to master_bookmark > "$TESTTMP/failed-remote-push" 2>&1
  [255]
  $ grep -F "abort: Server error: internal error: LandService::land_changesets failed with ThriftError" "$TESTTMP/failed-remote-push"
  abort: Server error: internal error: LandService::land_changesets failed with ThriftError
  $ grep -F "Connection refused" "$TESTTMP/failed-remote-push" > /dev/null && echo "connection refused"
  connection refused
  $ cat $SCUBA_LOGGING_PATH | jq 'select(.normal.log_tag=="failed_remote_pushrebase")' | format_single_scuba_sample_strip_server_info

If Land Service is off and edenapi_force_local_pushrebase is on, the remote pushrebase should run locally.
  $ killandwait $MONONOKE_PID
  $ cd $TESTTMP/mononoke-config
  $ cp repos/repo/server.toml.BACKUP repos/repo/server.toml
  $ cat >> repos/repo/server.toml << EOF
  > [pushrebase.remote_mode]
  > remote_land_service = { host_port = "$(land_service_address)" }
  > EOF
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:edenapi_force_local_pushrebase": true 
  >   }
  > }
  > EOF
  $ cd $TESTTMP
  $ start_and_wait_for_mononoke_server --scuba-log-file "$SCUBA_LOGGING_PATH"
  $ cd client1
  $ quiet hg push --to master_bookmark
  $ cat $SCUBA_LOGGING_PATH | jq 'select(.normal.log_tag=="failed_remote_pushrebase")' | format_single_scuba_sample_strip_server_info

If Land Service is off and fallback is on, the remote pushrebase should fail and be retried locally.
  $ quiet hg up $(mononoke_admin convert -R repo $X --from bonsai --to hg --derive)
  $ hg rebase -s . -d remote/master_bookmark
  rebasing fe45bfdf9de2 "X"
  $ killandwait $MONONOKE_PID
  $ cd $TESTTMP/mononoke-config
  $ cp repos/repo/server.toml.BACKUP repos/repo/server.toml
  $ cat >> repos/repo/server.toml << EOF
  > [pushrebase.remote_mode]
  > remote_land_service_local_fallback = { host_port = "$(land_service_address)" }
  > EOF
  $ merge_just_knobs <<EOF
  > {
  >   "bools": {
  >     "scm/mononoke:edenapi_force_local_pushrebase": false
  >   }
  > }
  > EOF
  $ cd $TESTTMP
  $ start_and_wait_for_mononoke_server --scuba-log-file "$SCUBA_LOGGING_PATH"
  $ cd client1
  $ quiet hg push --to master_bookmark
  $ jq -S 'select(.normal.log_tag=="failed_remote_pushrebase") | .normal | {bookmark_name, http_method, http_path, log_tag, msg}' < "$SCUBA_LOGGING_PATH"
  {
    "bookmark_name": "master_bookmark",
    "http_method": "POST",
    "http_path": "/repo/land",
    "log_tag": "failed_remote_pushrebase",
    "msg": "LandService::land_changesets failed with ThriftError"
  }
