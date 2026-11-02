# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ setup_common_config
  $ setup_configerator_configs
  $ start_and_wait_for_mononoke_server
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ hginit_treemanifest repo
  $ cd repo

Helper for making commit:
  $ function commit() { # the arg is used both for commit message and variable name
  >   hg commit -qAm $1 # create commit
  >   export COMMIT_$1="$(hg --debug id -i)" # save hash to variable
  > }

Four simple commits simulating master branch:
  $ echo "a" > a
  $ commit MASTER_1

  $ echo "b" > b
  $ commit MASTER_2

  $ echo "c" > c
  $ commit MASTER_3

  $ echo "d" > d
  $ commit MASTER_4


Branch B
  $ hg update -q $COMMIT_MASTER_2

  $ echo "b" >> b
  $ commit BRANCH_B_1

  $ echo "b" >> b
  $ commit BRANCH_B_2

  $ echo "b" >> b
  $ commit BRANCH_B_3

Branch C
  $ hg update -q $COMMIT_MASTER_3

  $ echo "c" >> c
  $ commit BRANCH_C_1

  $ echo "c" >> c
  $ commit BRANCH_C_2

Merge 1
  $ hg -q --config ui.allowmerge=True merge $COMMIT_BRANCH_B_3
  $ commit MERGE_1
  $ hg update -q $COMMIT_BRANCH_B_3
  $ hg -q --config ui.allowmerge=True merge $COMMIT_BRANCH_C_2
  $ commit MERGE_2

Unrelated commit
  $ hg update -q null
  $ echo "a" > u
  $ commit UNRELATED_1

  $ echo "b" > u
  $ commit UNRELATED_2

start SCS server
  $ cd ..
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

import testing repo to mononoke via push
  $ cd repo
  $ cat >> .hg/hgrc <<EOF
  > [paths]
  > default = mono:repo
  > EOF
  $ hg push --to master_bookmark --create -r "$COMMIT_MASTER_4" -q
  $ hg push --to branch_b --create -r "$COMMIT_MERGE_1" -q
  $ hg push --to branch_c --create -r "$COMMIT_MERGE_2" -q
  $ hg push --to unrelated --create -r "$COMMIT_UNRELATED_2" -q
  $ cd $TESTTMP

Common base tests
  $ RESULT=$(scsc common-base --repo repo -i $COMMIT_BRANCH_B_3 -i $COMMIT_BRANCH_C_2 -S hg)
  $ [[ "$COMMIT_MASTER_2" == "$RESULT" ]]
  $ RESULT=$(scsc common-base --repo repo -i $COMMIT_MASTER_4 -i $COMMIT_BRANCH_C_2 -S hg)
  $ [[ "$COMMIT_MASTER_3" == "$RESULT" ]]
  $ RESULT=$(scsc common-base --repo repo -i $COMMIT_MERGE_1 -i $COMMIT_MERGE_2 -S hg)
  $ [[ "$COMMIT_BRANCH_B_3" == "$RESULT" ]]
  $ RUST_BACKTRACE=1 scsc common-base --repo repo -i $COMMIT_UNRELATED_1 -i $COMMIT_MERGE_2 -S hg
  *: error: a common ancestor of commit id '77f0d9cb6615acc1c417fe8840381d38876e6a2b' and commit id 'f41d0e6d0b41c1c97d1c8e03b1a5fbbf36cecb21' does not exist (glob)
  
  [1]
