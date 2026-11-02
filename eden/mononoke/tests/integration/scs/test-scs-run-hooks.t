# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
Setup repo and SCS
  $ REPONAME="test/repo"
  $ setup_common_config
  $ cd "$TESTTMP/mononoke-config"
  $ cat >> repos/test%2Frepo/server.toml <<'CONFIG'
  > [[bookmarks]]
  > name="master"
  > [[bookmarks.hooks]]
  > hook_name="no_bad_filenames"
  > [[bookmarks.hooks]]
  > hook_name="block_commit_message_pattern"
  > [[hooks]]
  > name="no_bad_filenames"
  > bypass_pushvar="ALLOW_PYC=true"
  > config_json='''{
  >   "illegal_regex": ".+\\.pyc$",
  >   "illegal_filename_message": "ABORT: Illegal filename: '${filename}'. You cannot commit .pyc files."
  > }'''
  > [[hooks]]
  > name="block_commit_message_pattern"
  > log_only=true
  > config_json='''{
  >   "pattern": "([@]nocommit)",
  >   "message": "Message contains nocommit marker"
  > }'''
  > CONFIG
  $ testtool_drawdag -R test/repo <<EOF
  > A-B-C
  > # bookmark: B master
  > D
  > E
  > F
  > G
  > # modify: E file.pyc "You can't commit files with this extension"
  > # modify: F file1.py "You can commit files with this extension"
  > # modify: F file2.pyc "You can't commit files with this extension"
  > # modify: F file3.pyc "You can't commit files with this extension"
  > # modify: G file4pyc "You can commit files with their names ending in pyc"
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=4879da21c6ce6b7771bbc5b53c5aa13689b4117e7f9361364e15285a8616b076
  E=85d724a4dbadefdaa0bc19437e3e8e7d5549540c2ecd9c8205436600690ceb0c
  F=73df292dd5602f7c6a3ceaa56a7b6c56796f27ed5431ed97247a159570177070
  G=9c138f3414ea5e137edadfdafecd13cb1f6e6d68e7c63ff11e8aa4e710cc6607
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"
Test running hooks
  $ scsc info -R test/repo --bookmark master
  Commit: 80521a640a0c8f51dcc128c2658b224d595840ac
  Parent: 20ca2a4749a439b459125ef0f6a4f26e88ee7538
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Generation: 2
  
  B
  $ scsc run-hooks -R test/repo --commit-id $D --to master
  Hook outcomes when dry-run landing commit id '4879da21c6ce6b7771bbc5b53c5aa13689b4117e7f9361364e15285a8616b076' to bookmark master:
  
  block_commit_message_pattern => ACCEPTED
  no_bad_filenames => ACCEPTED
  $ scsc run-hooks -R test/repo --commit-id $E --to master
  Hook outcomes when dry-run landing commit id '85d724a4dbadefdaa0bc19437e3e8e7d5549540c2ecd9c8205436600690ceb0c' to bookmark master:
  
  block_commit_message_pattern => ACCEPTED
  no_bad_filenames => REJECTED: ABORT: Illegal filename: 'file.pyc'. You cannot commit .pyc files.
  $ scsc run-hooks -R test/repo --commit-id $F --to master
  Hook outcomes when dry-run landing commit id '73df292dd5602f7c6a3ceaa56a7b6c56796f27ed5431ed97247a159570177070' to bookmark master:
  
  block_commit_message_pattern => ACCEPTED
  no_bad_filenames => REJECTED: ABORT: Illegal filename: 'file2.pyc'. You cannot commit .pyc files.
  ABORT: Illegal filename: 'file3.pyc'. You cannot commit .pyc files.

  $ scsc run-hooks -R test/repo --commit-id $G --to master
  Hook outcomes when dry-run landing commit id '9c138f3414ea5e137edadfdafecd13cb1f6e6d68e7c63ff11e8aa4e710cc6607' to bookmark master:
  
  block_commit_message_pattern => ACCEPTED
  no_bad_filenames => ACCEPTED

  $ scsc --json run-hooks -R test/repo --commit-id $E --to master
  {"commit":"commit id '85d724a4dbadefdaa0bc19437e3e8e7d5549540c2ecd9c8205436600690ceb0c'","bookmark":"master","outcomes":{"block_commit_message_pattern":{"status":"Accepted"},"no_bad_filenames":{"status":"Rejected","reason":"ABORT: Illegal filename: 'file.pyc'. You cannot commit .pyc files."}}}
  $ scsc run-hooks -R test/repo --commit-id $E --to master --pushvar ALLOW_PYC=true
  Hook outcomes when dry-run landing commit id '85d724a4dbadefdaa0bc19437e3e8e7d5549540c2ecd9c8205436600690ceb0c' to bookmark master:
  
  block_commit_message_pattern => ACCEPTED

  $ scsc info -R test/repo --bookmark master
  Commit: 80521a640a0c8f51dcc128c2658b224d595840ac
  Parent: 20ca2a4749a439b459125ef0f6a4f26e88ee7538
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Generation: 2
  
  B

The pattern hook is log-only: a commit whose stored message trips it still
reports ACCEPTED by default, and --include-log-only-rejections reveals the
underlying verdict without changing anything else

  $ NOCOMMIT="@""nocommit"
  $ testtool_drawdag -R test/repo <<DRAWDAG
  > H
  > # message: H "this one has an ${NOCOMMIT} marker"
  > DRAWDAG
  H=* (glob)
  $ scsc --json run-hooks -R test/repo --commit-id "$H" --to master | jq -S .outcomes
  {
    "block_commit_message_pattern": {
      "status": "Accepted"
    },
    "no_bad_filenames": {
      "status": "Accepted"
    }
  }
  $ scsc --json run-hooks -R test/repo --commit-id "$H" --to master --include-log-only-rejections | jq -S .outcomes
  {
    "block_commit_message_pattern": {
      "reason": "Message contains nocommit marker",
      "status": "Rejected"
    },
    "no_bad_filenames": {
      "status": "Accepted"
    }
  }

The hooks evaluate an overridden message in place of the stored one: a clean
commit rejects when the override carries the marker, and the marker commit
passes when the override is clean

  $ scsc --json run-hooks -R test/repo --commit-id $D --to master --include-log-only-rejections --override-commit-message "now with an ${NOCOMMIT} marker" | jq -S .outcomes
  {
    "block_commit_message_pattern": {
      "reason": "Message contains nocommit marker",
      "status": "Rejected"
    },
    "no_bad_filenames": {
      "status": "Accepted"
    }
  }
  $ scsc --json run-hooks -R test/repo --commit-id "$H" --to master --include-log-only-rejections --override-commit-message "a clean message" | jq -S .outcomes
  {
    "block_commit_message_pattern": {
      "status": "Accepted"
    },
    "no_bad_filenames": {
      "status": "Accepted"
    }
  }

File-based hooks still resolve the stored commit's file list when the message
is overridden (the override keeps the changeset id, so id-based lookups hit
the real commit)

  $ scsc --json run-hooks -R test/repo --commit-id $E --to master --include-log-only-rejections --override-commit-message "a clean message" | jq -S .outcomes
  {
    "block_commit_message_pattern": {
      "status": "Accepted"
    },
    "no_bad_filenames": {
      "reason": "ABORT: Illegal filename: 'file.pyc'. You cannot commit .pyc files.",
      "status": "Rejected"
    }
  }
