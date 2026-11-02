# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

  $ setup_common_config
  $ setup_configerator_configs
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ testtool_drawdag --print-hg-hashes -R repo --derive-all <<EOF
  > A-B
  > # modify: A a "a"
  > # modify: B b "b"
  > # message: A "first commit"
  > # message: B "commit with svnrev"
  > # extra: B convert_revision "svn:22222222-aaaa-0000-aaaa-ddddddddcccc/repo/trunk/project@2077"
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)

  $ export HG_HASH_1=$A
  $ export HG_HASH_2=$B

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

  $ echo $HG_HASH_1 > hash_list
  $ echo $HG_HASH_2 > hash_list
  $ backfill_mapping --svnrev hash_list

check that mapping is populated
  $ echo ${HG_HASH_1^^}
  05210B1578FA5B7ACA70C9745679DDA404A2B274
  $ echo ${HG_HASH_2^^}
  E536CF0DB9823431D7533C8D1834A28BB9EC9F96

  $ get_bonsai_svnrev_mapping
  91D78FA976068CD7F0FDC7042871D4D17A83696FF1110F5B54F269C54AA50730|2077

  $ get_bonsai_hg_mapping
  91D78FA976068CD7F0FDC7042871D4D17A83696FF1110F5B54F269C54AA50730|E536CF0DB9823431D7533C8D1834A28BB9EC9F96
  DE47E0A8F39F1CD7F534FB951AEED9E006F579D23EAEBC00D1FB51EAC5A29131|05210B1578FA5B7ACA70C9745679DDA404A2B274

lookup using bookmark
  $ scsc lookup --repo repo --hg-commit-id $HG_HASH_1 -S svnrev
  *scsc: error: hg commit id '05210b1578fa5b7aca70c9745679dda404a2b274' does not have any 'svnrev' ids (glob)
  [1]
  $ scsc lookup --repo repo --hg-commit-id $HG_HASH_2 -S svnrev
  2077

  $ scsc lookup --repo repo --svnrev 2077 -S hg
  e536cf0db9823431d7533c8d1834a28bb9ec9f96
