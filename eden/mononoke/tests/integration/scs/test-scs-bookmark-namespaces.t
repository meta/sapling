# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

Test repo_resolve_bookmark with namespaces parameter

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ setup_common_config
  $ cd $TESTTMP

setup repo
  $ quiet testtool_drawdag -R repo --no-default-files <<EOF
  > A
  > # message: A "initial commit"
  > # author: A test
  > # modify: A file "content\n"
  > # bookmark: A main
  > # bookmark: A heads/main
  > # bookmark: A tags/main
  > # bookmark: A heads/feature-branch
  > # bookmark: A tags/v1.0.0
  > EOF


start SCS server
  $ start_and_wait_for_scs_server

Test 1: namespaces parameter with match in first namespace (empty namespace)
  $ scsc --json lookup --repo repo -S bonsai --bookmark-name main --namespaces "" "heads/" "tags/"
  {"exists":true,"ids":{"bonsai":"*"}} (glob)

Test 2: namespaces parameter with match in second namespace (heads/)
Remove the bare "main" bookmark so only heads/main and tags/main exist
  $ mononoke_admin bookmarks -R repo delete main
  Deleting publishing bookmark main at 2866d7f175fdbe990339bafead93e28311285dbda0bf918e0a4b8d1ca3a5e415

  $ scsc --json lookup --repo repo -S bonsai --bookmark-name main --namespaces "" "heads/" "tags/"
  {"exists":true,"ids":{"bonsai":"*"}} (glob)

Test 3: namespaces parameter with match in third namespace (tags/)
Remove heads/main so only tags/main exists
  $ mononoke_admin bookmarks -R repo delete heads/main
  Deleting publishing bookmark heads/main at 2866d7f175fdbe990339bafead93e28311285dbda0bf918e0a4b8d1ca3a5e415

  $ scsc --json lookup --repo repo -S bonsai --bookmark-name main --namespaces "" "heads/" "tags/"
  {"exists":true,"ids":{"bonsai":"*"}} (glob)

Test 4: namespaces parameter with no matches
  $ scsc --json lookup --repo repo -S bonsai --bookmark-name nonexistent --namespaces "" "heads/" "tags/"
  {"exists":false,"ids":{}}

Test 5: Check that order matters - first match wins
Re-create bookmarks in different namespaces with divergent commits
  $ testtool_drawdag -R repo --print-hg-hashes --no-default-files <<EOF
  > C
  > |
  > B
  > |
  > A
  > # message: B "second commit"
  > # author: B test
  > # modify: B file2 "content2\n"
  > # bookmark: B release
  > # message: C "different commit"
  > # author: C test
  > # modify: C file3 "content3\n"
  > # bookmark: C heads/release
  > EOF
  A=5dce5fe8bca8dd081ee2f8c35e50ee70a916016b
  B=56c398ca447d7a763cfa2b7800ac5122cef552e8
  C=2b4bb912c5370db94d17e13e3bc3976dffe38c71

Query with "" first in namespaces - should return "release" (not "heads/release")
  $ scsc lookup --repo repo -S hg --bookmark-name release --namespaces "" "heads/" | grep "$B"
  * (glob)

Query with "heads/" first in namespaces - should return "heads/release" (not "release")
  $ scsc lookup --repo repo -S hg --bookmark-name release --namespaces "heads/" "" | grep "$C"
  * (glob)

Test 6: Without namespaces parameter (original behavior)
  $ scsc --json lookup --repo repo -S bonsai -B release
  {"exists":true,"ids":{"bonsai":"*"}} (glob)

Test 7: Test with feature branch (only in heads/ namespace)
  $ scsc --json lookup --repo repo -S bonsai --bookmark-name feature-branch --namespaces "" "heads/" "tags/"
  {"exists":true,"ids":{"bonsai":"*"}} (glob)

Test 8: Test with tag (only in tags/ namespace)
  $ scsc --json lookup --repo repo -S bonsai --bookmark-name v1.0.0 --namespaces "" "heads/" "tags/"
  {"exists":true,"ids":{"bonsai":"*"}} (glob)
