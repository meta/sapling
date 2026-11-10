# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ POPULATE_GIT_MAPPING=1 setup_common_config
  $ setup_configerator_configs
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ testtool_drawdag -R repo --derive-all --print-hg-hashes --no-default-files <<'EOF'
  > A-B-C-GREV-GIT
  > B-MOVE-SUBDIR
  > # modify: A "a" "a\nb\nc\nd\ne\n"
  > # modify: B "b" "a\nb\nd\ne\nf\n"
  > # modify: C "b" "b\nc\nd\ne\nf\n"
  > # modify: C "binary" "\x00 10\n"
  > # modify: GREV "c" "c\n"
  > # modify: GIT "d" "d\n"
  > # extra: GREV global_rev "9999999999"
  > # extra: GIT convert_revision "37b0a167e07f2b84149c918cec818ffeb183dddd"
  > # extra: GIT hg-git-rename-source "git"
  > # copy: MOVE "moved_a" "a\nb\nc\nd\ne\nx\n" B "a"
  > # copy: MOVE "copied_b" "a\nb\nd\ne\nf\n" B "b"
  > # delete: MOVE "a"
  > # copy: SUBDIR "dir_a/a" "a\nb\nc\nd\ne\nx\nx\n" MOVE "moved_a"
  > # modify: SUBDIR "dir_b/y" "y\n"
  > # delete: SUBDIR "moved_a"
  > # bookmark: B BOOKMARK_B
  > # bookmark: GREV BOOKMARK_C
  > # bookmark: GIT BOOKMARK_D
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  GIT=* (glob)
  GREV=* (glob)
  MOVE=* (glob)
  SUBDIR=* (glob)

Populate globalrev mapping:
  $ GREV_BONSAI=$(mononoke_admin convert -R repo --from hg --to bonsai "$GREV" 2>/dev/null)
  $ set_bonsai_globalrev_mapping "$REPOID" "$GREV_BONSAI" 9999999999

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

Compute git hash for use in lookups:
  $ GIT_SHA=$(scsc lookup --repo repo -B BOOKMARK_D -S git)

lookup using bookmark
  $ scsc lookup --repo repo -B BOOKMARK_C -S bonsai
  * (glob)

lookup, commit with git
  $ scsc lookup --repo repo  -B BOOKMARK_D -S bonsai,hg,globalrev,git
  bonsai=* (glob)
  git=* (glob)
  hg=* (glob)

lookup, commit with globalrev
  $ scsc lookup --repo repo -B BOOKMARK_C -S bonsai,hg,globalrev,git
  bonsai=* (glob)
  git=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using bonsai to identify commit
  $ scsc lookup --repo repo --bonsai-id "$GREV_BONSAI" -S bonsai,hg,globalrev
  bonsai=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using globalrev to identify commit
  $ scsc lookup --repo repo --globalrev 9999999999 -S bonsai,hg,globalrev
  bonsai=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using git to identify commit
  $ scsc lookup --repo repo --git "$GIT_SHA" -S bonsai,hg,globalrev
  bonsai=* (glob)
  hg=* (glob)

lookup using hg to identify commit
  $ scsc lookup --repo repo --hg-commit-id "$GREV" -S bonsai,hg,globalrev
  bonsai=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using bonsai needed resolving to identify commit
  $ scsc lookup --repo repo -i "$GREV_BONSAI" -S bonsai,hg,globalrev
  bonsai=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using bonsai prefix needed resolving to identify commit
  $ GREV_BONSAI_PREFIX=${GREV_BONSAI:0:20}
  $ scsc lookup --repo repo -i "$GREV_BONSAI_PREFIX" -S bonsai,hg,globalrev
  bonsai=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using globalrev needed resolving to identify commit
  $ scsc lookup --repo repo -i 9999999999 -S bonsai,hg,globalrev
  bonsai=* (glob)
  globalrev=9999999999
  hg=* (glob)

lookup using hg needed resolving to identify commit
  $ scsc lookup --repo repo -i "$SUBDIR" -S bonsai,hg,globalrev
  bonsai=* (glob)
  hg=* (glob)

lookup using hg prefix needed resolving to identify commit
  $ SUBDIR_PREFIX=${SUBDIR:0:8}
  $ scsc lookup --repo repo -i "$SUBDIR_PREFIX" -S bonsai,hg,globalrev
  bonsai=* (glob)
  hg=* (glob)

lookup using hg prefix needed resolving to identify commit (ambiguous case)
  $ scsc lookup --repo repo -i 3 -S bonsai,hg,globalrev
  note: several * commits with the prefix '3' exist (glob)
  *scsc: error: commit not found: 3 (glob)
  [1]

lookup using git prefix needed resolving to identify commit
  $ GIT_SHA_PREFIX=${GIT_SHA:0:7}
  $ scsc lookup --repo repo -i "$GIT_SHA_PREFIX" -S git
  * (glob)

multiple lookup using git and hg to identify commits
  $ scsc lookup --repo repo --git "$GIT_SHA" --hg-commit-id "$GREV" -S bonsai,hg,globalrev
  git sha1 '*': bonsai=* hg=* (glob)
  hg commit id '*': bonsai=* globalrev=9999999999 hg=* (glob)

multiple lookup using prefix git and hg aborts execution if one of the commits fails to resolve
  $ scsc lookup --repo repo --git "$GIT_SHA_PREFIX" --hg-commit-id "$GREV" -S bonsai,hg,globalrev
  *scsc: error: error: hex input length must be even (glob)
  [1]

multiple lookup using prefix git and hg to identify commits
  $ GREV_PREFIX=${GREV:0:8}
  $ scsc lookup --repo repo -i "$GIT_SHA_PREFIX" -i "$GREV_PREFIX" -S bonsai,hg,globalrev
  commit id '*': bonsai=* hg=* (glob)
  commit id '*': bonsai=* globalrev=9999999999 hg=* (glob)

multiple lookup using existing and non existing commits
  $ scsc lookup --repo repo --git "$GIT_SHA" --hg-commit-id 05f4a4c94d4625560e40e6e3c04775dc4f3a7fa6 -S git,bonsai,hg,globalrev
  git sha1 '*': bonsai=* git=* hg=* (glob)

# An error in the resolution of any of the commit Ids aborts the lookup
multiple lookup using git and hg to identify commits (ambiguous case)
  $ scsc lookup --repo repo -i 3 -i 2 -S bonsai,hg,globalrev
  note: several * commits with the prefix '3' exist (glob)
  *scsc: error: commit not found: 3 (glob)
  [1]
