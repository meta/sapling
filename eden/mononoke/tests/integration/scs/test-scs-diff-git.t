# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
Disable Mercurial types as they do not support git submodules
  $ export DISABLED_DERIVED_DATA="filenodes hgchangesets hg_augmented_manifests"
Common setup with boring repo
  $ setup_common_config blob_files
  $ export ENABLE_BOOKMARK_CACHE=1
  $ testtool_drawdag -R repo << EOF
  > A-B-C
  > # bookmark: C main
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2

Start the source control service

  $ start_and_wait_for_scs_server

Check that `scsc diff` work when hg derivation is disabled
  $ scsc diff -R repo -i $C
  diff --git a/C b/C
  new file mode 100644
  --- /dev/null
  +++ b/C
  @@ -0,0 +1,1 @@
  +C
  \ No newline at end of file

Show behaviour with different commit schemes setting.
1. hg commit scheme - won't fail as don't need that id so it's OK to have it empty
  $ scsc diff -R repo -i $C -S hg
  diff --git a/C b/C
  new file mode 100644
  --- /dev/null
  +++ b/C
  @@ -0,0 +1,1 @@
  +C
  \ No newline at end of file
2. git commit scheme - should just work
  $ scsc diff -R repo -i $C -S git
  diff --git a/C b/C
  new file mode 100644
  --- /dev/null
  +++ b/C
  @@ -0,0 +1,1 @@
  +C
  \ No newline at end of file
3. no commit scheme
  $ scsc diff -R repo -i $C
  diff --git a/C b/C
  new file mode 100644
  --- /dev/null
  +++ b/C
  @@ -0,0 +1,1 @@
  +C
  \ No newline at end of file
