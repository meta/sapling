# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Set up local hgrc and Mononoke config.
  $ setup_common_config
  $ cd $TESTTMP

Initialize test repo.
  $ hginit_treemanifest repo
  $ cd repo
  $ testtool_drawdag -R repo --print-hg-hashes <<EOF
  > COMMIT_2
  > |
  > COMMIT_1
  > # modify: COMMIT_1 "test.txt" "my commit message\n"
  > # modify: COMMIT_2 "copy.txt" "my commit message\n"
  > # message: COMMIT_1 "add test.txt"
  > # message: COMMIT_2 "copy test.txt to test2.txt"
  > # bookmark: COMMIT_2 master_bookmark
  > EOF
  COMMIT_1=2c73711572dcc5fba150bc86885bed40a3950176
  COMMIT_2=87d56d8162fe716b567ccf245ad56fa9a90b5069

Import test repo.
  $ cd ..

Start up SaplingRemoteAPI server.
  $ start_and_wait_for_mononoke_server
Check response.
  $ hg debugapi mono:repo -e commitdata -i "['$COMMIT_1','$COMMIT_2']" --sort
  [{"hgid": bin("2c73711572dcc5fba150bc86885bed40a3950176"),
    "revlog_data": b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\03aa4499a34ac1ed79157ffad61e912c79a64989b\nauthor\n0 0\nCOMMIT_1\ntest.txt\n\nadd test.txt"},
   {"hgid": bin("87d56d8162fe716b567ccf245ad56fa9a90b5069"),
    "revlog_data": b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0,sq\x15r\xdc\xc5\xfb\xa1P\xbc\x86\x88[\xed@\xa3\x95\x01v30dbf93b36115c8d99b722ff0a0556f0cff4d883\nauthor\n0 0\nCOMMIT_2\ncopy.txt\n\ncopy test.txt to test2.txt"}]
