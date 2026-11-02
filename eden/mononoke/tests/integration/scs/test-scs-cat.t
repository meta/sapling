# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
Setup repo
  $ REPONAME="test/repo"
  $ setup_common_config
  $ testtool_drawdag --repo-name test/repo << EOF
  > A-B-C-D
  > # modify: B dir/file content
  > # modify: B dir/other_file other_content
  > # modify: D dir/file new_content
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=33179ed04a11d3d0057eb080f263c4bc0e77f87bb03dfd8acc6fc39b82b8ee7b
  C=4fef8c3c5f93e13d3d21c20e9c6ebd90a5f3d2165492a69706ad8ae95ef9fb41
  D=bbdc935dd71cd43e5154810b27a49240438a28dd1d9c2c48f99041a6b554c90a
  $ start_and_wait_for_scs_server
Test with scsc cat
  $ scsc cat -R test/repo -p dir/file --bonsai-id $B
  content (no-eol)
  $ scsc cat -R test/repo -p dir/other_file --bonsai-id $B
  other_content (no-eol)
  $ scsc cat -R test/repo -p dir/file --bonsai-id $C
  content (no-eol)
  $ scsc cat -R test/repo -p dir/file --bonsai-id $D
  new_content (no-eol)
  $ scsc cat -R test/repo -p dir/nonexisting_file --bonsai-id $D
  *scsc: error: SourceControlService::file_content_chunk failed with request_error(RequestError): RequestError: file not found (repo=test/repo commit=* path=dir/nonexisting_file) (glob)
  [1]
  $ scsc cat -R test/repo -p dir --bonsai-id $D
  *scsc: error: SourceControlService::file_content_chunk failed with request_error(RequestError): RequestError: file not found (repo=test/repo commit=* path=dir) (glob)
  [1]
