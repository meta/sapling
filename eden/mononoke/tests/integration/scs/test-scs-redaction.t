# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setconfig ui.ignorerevnum=false

setup configuration

  $ REPOTYPE="blob_files"
  $ setup_common_config $REPOTYPE

  $ cd $TESTTMP

setup repo with testtool_drawdag
  $ testtool_drawdag -R repo --no-default-files --derive-all --print-hg-hashes <<EOF
  > A-B
  > # modify: A "a" "content_a"
  > # modify: B "b" "content_b"
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)

start mononoke
  $ start_and_wait_for_mononoke_server

setup repo-pull
  $ hg clone -q mono:repo repo-pull --noupdate
  $ cd repo-pull
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > EOF

  $ hg pull -q
  $ COMMIT_B=$(hg log -r 'desc("B")' -T '{node}')
  $ hg up -q $COMMIT_B

Redact file 'b' in commit '$COMMIT_B'
  $ mononoke_admin redaction create-key-list -R repo -i $COMMIT_B b --main-bookmark master_bookmark --force --output-file rs_0 --skip-aws-sync
  Checking redacted content doesn't exist in 'master_bookmark' bookmark
  Redacted content in main bookmark: b content.blake2.* (glob)
  Creating key list despite 1 files being redacted in the main bookmark (master_bookmark) (--force)
  Redaction saved as: * (glob)
  To finish the redaction process, you need to commit this id to scm/mononoke/redaction/redaction_sets.cconf in configerator

  $ cat > "$REDACTION_CONF/redaction_sets" <<EOF
  > {
  >  "all_redactions": [
  >    {"reason": "T12345", "id": "$(cat rs_0)", "enforce": true}
  >  ]
  > }
  > EOF
  $ rm rs_0

The file should now be marked as redacted
  $ mononoke_admin redaction list -R repo -i $COMMIT_B
  Searching for redacted paths in * (glob)
  Found 1 redacted paths
  T12345              : b

Restart mononoke to pick up the redaction config
  $ killandwait $MONONOKE_PID
  $ rm -rf "$TESTTMP/mononoke-config"
  $ setup_common_config blob_files
  $ start_and_wait_for_mononoke_server

start scs server
  $ start_and_wait_for_scs_server

Check that scsc returns the correct error for redacted content
  $ scsc cat -R repo -i $COMMIT_B -p b
  *scsc: error: SourceControlService::file_content_chunk failed with request_error(RequestError): RequestError: The blob content.blake2.* was redacted due to T12345 (glob)
  [1]

File 'a' should still be accessible since it's not redacted
  $ scsc cat -R repo -i $COMMIT_B -p a
  content_a (no-eol)
