# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

With `scm/mononoke:pushrebase_land_stack_validation` at 1, `land_stack` checks
a stack without merges, logs to Scuba what it would reject, and lands it as it
would with the checks off. Stacks with merges are checked and rejected.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setup_common_config
  $ setup_configerator_configs
  $ merge_just_knobs <<EOF
  > {"ints": {"scm/mononoke:pushrebase_land_stack_validation": 1}}
  > EOF
  $ cd $TESTTMP

Master is A-B-X-T, where X changes no files. S is a draft child of X. The
release branch R is public, and P is a draft child of it. M merges S and P.
  $ testtool_drawdag -R repo --print-hg-hashes << 'EOF'
  > A----B----X----T
  >  \         \
  >   \         S---M
  >    R---P-------/
  > # bookmark: T master_bookmark
  > # bookmark: R release
  > # default_files: false
  > # modify: A a "a"
  > # modify: B b "b"
  > # modify: T t "t"
  > # modify: S s "s"
  > # modify: R r "r"
  > # modify: P p "p"
  > # modify: M m "m"
  > EOF
  A=caad10acff5bb8a83fa7d320a12be32588a7df52
  B=113ea1a8525c8625a52eca270d9873c80e1c888a
  M=33f8bca4d24d1c0f155db7f58cb7f60b231f2e3c
  P=e8a6e577d82e3262b3587fa71625448c8a20574e
  R=576c4bfe1019ee84da23fa8aff77f76ac2848f66
  S=03c4abec8915fa6f94d1ad41017894bf486c8929
  T=f4cab1d2ee3589bf1a78a1a739bb50d0d6851ce6
  X=62fed80218573827e4e7e94c328f08b6e8dd96e9

  $ start_and_wait_for_mononoke_server --scuba-log-file "$TESTTMP/scuba.json"
  $ hg clone -q mono:repo repo --noupdate
  $ cd repo
  $ hg pull -q -r $S -r $P -r $M
  $ rows() {
  >   jq -c 'select(.normal.log_tag // "" | startswith("land_stack validation")) | .normal + .int + .normvector | {log_tag, land_stack_validation, land_stack_validation_check, land_stack_merge_stack, land_stack_bookmark, land_stack_base, land_stack_head, land_stack_merge_bases, land_stack_public_commits}' "$TESTTMP/scuba.json"
  > }

S is based on B instead of X, its merge base with master, so the stack
includes X, which is public. X changes no files, so pushrebase lands X and S
without conflicts, as it does with the checks off. The mismatch is logged:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$S'" -i "'$B'"
  {"data": {"Ok": {"new_head": bin("1b4b25ed80cb33dcab1ddde207d522414b327778"),
                   "old_to_new_hgids": {bin("03c4abec8915fa6f94d1ad41017894bf486c8929"): bin("1b4b25ed80cb33dcab1ddde207d522414b327778"),
                                        bin("62fed80218573827e4e7e94c328f08b6e8dd96e9"): bin("155f024060b38b7d62944fe36e33a7c3c8865270")}}}}
  $ rows
  {"log_tag":"land_stack validation would reject","land_stack_validation":"shadow","land_stack_validation_check":"merge_base_mismatch","land_stack_merge_stack":"false","land_stack_bookmark":"master_bookmark","land_stack_base":"e80d548832f2cf740aa885154a2b347d754d095585bd68ccec60077e5e9a9734","land_stack_head":"9d64ef46fded6a2e0c65cb5bc6d481a9aa6ddb54e20bbb20f8bc05e4efa6bd42","land_stack_merge_bases":["9cf96963a59167a724ca3d6e28b869af8dc119fdf63e753e5a8e8427b19637ed"],"land_stack_public_commits":1}

P is based on A, its merge base with master, so the stack includes R, which is
public. It lands, rebasing R, and the public commit is logged:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$P'" -i "'$A'"
  {"data": {"Ok": {"new_head": bin("d7f97ca69bb93290058c697b5089a4abdcbe33f7"),
                   "old_to_new_hgids": {bin("576c4bfe1019ee84da23fa8aff77f76ac2848f66"): bin("87c70be13b16081ef67f8389cb5f20a02b027613"),
                                        bin("e8a6e577d82e3262b3587fa71625448c8a20574e"): bin("d7f97ca69bb93290058c697b5089a4abdcbe33f7")}}}}
  $ rows | tail -1
  {"log_tag":"land_stack validation would reject","land_stack_validation":"shadow","land_stack_validation_check":"public_commits","land_stack_merge_stack":"false","land_stack_bookmark":"master_bookmark","land_stack_base":"4d0fcad6f5887e5b78b78aeb6e9ba496b365256130977e410fa732cca60796d6","land_stack_head":"83e11337714e673642a566e292e19ae952cb4beb546589ccae7a697d8a4f33fc","land_stack_merge_bases":["4d0fcad6f5887e5b78b78aeb6e9ba496b365256130977e410fa732cca60796d6"],"land_stack_public_commits":1}

A stack with a merge is always checked, so M, based on B instead of X, is
rejected, and the rejection is logged:
  $ hg debugapi -e landstack -i "'master_bookmark'" -i "'$M'" -i "'$B'"
  {"data": {"Err": {"code": 0,
                    "message": "invalid request: bookmark 'master_bookmark' has moved incompatibly with this push: the merge base of 'master_bookmark' and 33f8bca4d24d1c0f155db7f58cb7f60b231f2e3c is 62fed80218573827e4e7e94c328f08b6e8dd96e9, but the push is based on 113ea1a8525c8625a52eca270d9873c80e1c888a. Pull and retry"}}}
  $ rows | tail -1
  {"log_tag":"land_stack validation rejected","land_stack_validation":"shadow","land_stack_validation_check":"merge_base_mismatch","land_stack_merge_stack":"true","land_stack_bookmark":"master_bookmark","land_stack_base":"e80d548832f2cf740aa885154a2b347d754d095585bd68ccec60077e5e9a9734","land_stack_head":"72023058e58f4eb7ac3e40141547a08ad0fab30afe1052a3ad3130fd3837c3d8","land_stack_merge_bases":["9cf96963a59167a724ca3d6e28b869af8dc119fdf63e753e5a8e8427b19637ed"],"land_stack_public_commits":1}
