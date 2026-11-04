# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Override subtree key to enable non-test subtree extra
  $ setconfig subtree.use-prod-subtree-key=True
  $ setconfig push.edenapi=true
  $ setconfig subtree.min-path-depth=1
  $ enable amend
  $ ADDITIONAL_DERIVED_DATA="content_manifests" setup_common_config blob_files

  $ cd $TESTTMP
  $ git init -q gitrepo
  $ cd gitrepo
  $ git config core.autocrlf false
  $ echo 1 > alpha
  $ git add alpha
  $ git commit -q -malpha
  $ mkdir dir1
  $ echo "b1" > dir1/beta
  $ git add dir1/beta
  $ git commit -q -mbeta
  $ echo "b2" > dir1/beta
  $ git add dir1/beta
  $ git commit -q -m "update beta"
  $ cd $TESTTMP
  $ export GIT_URL=git+file://$TESTTMP/gitrepo
  $ GIT_COMMIT=$(cd gitrepo && git rev-parse HEAD)
  $ GIT_IMPORT_COMMIT=$(cd gitrepo && git rev-parse HEAD~1)

  $ testtool_drawdag -R repo --derive-all --no-default-files << EOF
  > A-B-C
  > # modify: A foo/file1 "a1\n"
  > # modify: A foo/dir1/file3 "a3 (i)\na3 (ii)\na3 (iii)\n"
  > # modify: A foo/dir1/file4 "a4\n"
  > # copy: B foo/file2 "a1\nb2\n" A foo/file1
  > # delete: B foo/file1
  > # modify: B foo/dir1/file3 "a3 (i)\nb3 (ii)\na3 (iii)\nb3 (iv)\n"
  > # modify: C foo/file2 "a1\nb2\nc2\n"
  > # bookmark: C master_bookmark
  > EOF
  A=983a2c5f3f114b711fee36f465c9eb90cd87bf17657a4739b9fe4647150658db
  B=7264b8efda8fb8d70168118abc941343dae194786452a15ce32af044eb607c53
  C=1380a5d11933dd581b3b1a4e94b19c913a4a3436c1562e0ba4674906b38dbb20

  $ start_and_wait_for_mononoke_server
  $ hg clone -q mono:repo repo
  $ cd repo

  $ hg subtree copy -r .^ --from-path foo --to-path bar
  copying foo to bar

  $ hg push -r . --to master_bookmark
  pushing rev 67a556b7d7d3 to destination https://localhost:$LOCAL_PORT/edenapi/ bookmark master_bookmark
  edenapi: queue 1 commit for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 3 trees for upload
  edenapi: uploaded 3 trees
  edenapi: uploaded 1 changeset
  pushrebasing stack (d29591b3c81c, 67a556b7d7d3] (1 commit) to remote bookmark master_bookmark
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master_bookmark to 67a556b7d7d3

  $ D=$(hg whereami)

  $ echo "e3 (v) {foo}" >> foo/dir1/file3
  $ echo "e2 {bar}" >> bar/file2
  $ hg commit -qm E
  $ hg push -q -r . --to master_bookmark
  $ E=$(hg whereami)

  $ hg subtree copy -r . --from-path foo --to-path baz
  copying foo to baz
  $ echo "f4 {baz}" >> baz/dir1/file4
  $ hg amend -q
  $ hg push -q -r . --to master_bookmark
  $ F=$(hg whereami)

  $ echo "g3 (vi) {bar}" >> bar/dir1/file3
  $ hg commit -qm "G"
  $ hg push -q -r . --to master_bookmark
  $ G=$(hg whereami)

  $ echo "h3 (vii) {bar}" >> bar/dir1/file3
  $ hg commit -qm "H"
  $ hg push -q -r . --to master_bookmark
  $ H=$(hg whereami)
  $ hg log -G -T '{node|short} {desc}\n'
  @  038130224609 H
  │
  o  d883cb4b3b1c G
  │
  o  c8d50c74f28b Subtree copy from 18dc0fef93d410fa82d06640fdf7cb4c3b14d903
  │  - Copied path foo to baz
  o  18dc0fef93d4 E
  │
  o  67a556b7d7d3 Subtree copy from 93b5138860f0eaaf84498fe9cf9a5fcf710edc91
  │  - Copied path foo to bar
  o  d29591b3c81c C
  │
  o  93b5138860f0 B
  │
  o  1c58c52a6ce2 A
  

  $ hg subtree merge -r .^ --from-path bar --to-path baz
  searching for merge base ...
  found the last subtree copy commit 67a556b7d7d3
  merge base: 93b5138860f0
  merging baz/dir1/file3 and bar/dir1/file3 to baz/dir1/file3
  warning: 1 conflicts while merging baz/dir1/file3! (edit, then use 'hg resolve --mark')
  merging baz/file2 and bar/file2 to baz/file2
  warning: 1 conflicts while merging baz/file2! (edit, then use 'hg resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 2 files unresolved
  use 'hg resolve' to retry unresolved file merges or 'hg goto -C .' to abandon
  [1]
  $ hg diff
  diff -r 038130224609 baz/dir1/file3
  --- a/baz/dir1/file3	Thu Jan 01 00:00:00 1970 +0000
  +++ b/baz/dir1/file3	Thu Jan 01 00:00:00 1970 +0000
  @@ -2,4 +2,8 @@
   b3 (ii)
   a3 (iii)
   b3 (iv)
  +<<<<<<< working copy: 038130224609 - test: H
   e3 (v) {foo}
  +=======
  +g3 (vi) {bar}
  +>>>>>>> merge rev:    d883cb4b3b1c - test: G
  diff -r 038130224609 baz/file2
  --- a/baz/file2	Thu Jan 01 00:00:00 1970 +0000
  +++ b/baz/file2	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,3 +1,7 @@
   a1
   b2
  +<<<<<<< working copy: 038130224609 - test: H
   c2
  +=======
  +e2 {bar}
  +>>>>>>> merge rev:    d883cb4b3b1c - test: G
  $ cat > baz/dir1/file3 <<EOF
  > a3 (i)
  > b3 (ii)
  > a3 (iii)
  > i3 (merge) {baz}
  > b3 (iv)
  > e3 (v) {foo}
  > g3 (vi) {bar}
  > EOF
  $ cat > baz/fileX << EOF
  > additional file
  > EOF
  $ hg add baz/fileX
  $ cat > baz/file2 <<EOF
  > a1
  > b2
  > c2
  > EOF
  $ hg resolve --mark baz/dir1/file3 baz/file2
  (no more unresolved files)
  $ hg commit -qm "I"
  $ hg push -q -r . --to master_bookmark
  $ I=$(hg whereami)

  $ echo "additional change" >> baz/fileX
  $ hg commit -qm "J"
  $ hg push -q -r . --to master_bookmark
  $ J=$(hg whereami)

  $ hg subtree copy -r .^ --from-path baz --to-path qu/ux
  copying baz to qu/ux
  $ hg rm qu/ux/fileX
  $ hg amend -q
  $ hg push -q -r . --to master_bookmark
  $ K=$(hg whereami)

  $ hg subtree import --url $GIT_URL --rev $GIT_IMPORT_COMMIT --to-path xgitrepo -m "import gitrepo to xgitrepo"
  creating git repo at $TESTTMP/cachepath/gitrepos/* (glob)
  From file://$TESTTMP/gitrepo
   * [new ref]         0b468b5eb6b8ea3401c5bb6fafbd190ed01506e2 -> remote/master_bookmark
   * [new ref]         d3bd984c3a882240b7b1b06e6debd5e564ba3c58 -> refs/visibleheads/d3bd984c3a882240b7b1b06e6debd5e564ba3c58
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  copying / to xgitrepo
  $ hg push -q -r . --to master_bookmark
  $ L=$(hg whereami)

  $ hg subtree merge --url $GIT_URL --rev $GIT_COMMIT --to-path xgitrepo
  using cached git repo at $TESTTMP/cachepath/gitrepos/* (glob)
  searching for merge base ...
  found the last subtree import commit * (glob)
  merge base: d3bd984c3a88
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (subtree merge, don't forget to commit)
  $ hg ci -m "xmerge gitrepo to xgitrepo"
  $ hg push -q -r . --to master_bookmark
  $ M=$(hg whereami)

  $ mononoke_admin derived-data -R repo derive -T test_manifests -T test_sharded_manifests --all-bookmarks
  $ mononoke_admin derived-data -R repo derive -T hgchangesets -T filenodes -T ccsm -T unodes -T blame --all-bookmarks
  $ mononoke_admin derived-data -R repo derive -T skeleton_manifests -T skeleton_manifests_v2 -T bssm_v3 --all-bookmarks
  $ mononoke_admin derived-data -R repo derive -T content_manifests --all-bookmarks
  $ mononoke_admin derived-data -R repo derive -T git_commits -T git_delta_manifests_v2 --all-bookmarks
  $ mononoke_admin derived-data -R repo list-manifest -i $K -r -t content-manifests | sort
  bar/dir1/file3	31e18005b0d0d1d6f6d6a5dd2947814c75d2779a6e3b2520275b6d28320d0bb5	type=regular	size=61
  bar/dir1/file4	983aa47751a102ff612e7a4c22334f55ca7b096018af300de31a2bc0a354e769	type=regular	size=3
  bar/file2	be309eaa94d587a1e672a43540edcd08fd50a33f8ce2b623694365aaa0657360	type=regular	size=15
  baz/dir1/file3	ab4cfaaae9130d5fe0da336c11e9bbb1548999e220d7f5b8a2141b492497a1bd	type=regular	size=76
  baz/dir1/file4	bb3283f3abf1288e1daae489d17bc43577f2fc62726c322d75b6cd062ec5a6c5	type=regular	size=12
  baz/file2	a04945366726ef3cec5979bc209418349dcc55450c4fbdbf72c7df9a6d291eab	type=regular	size=9
  baz/fileX	2c8364cee11abc7725b0213567ba464dcbfca388143c790c5e6068aed82a9e43	type=regular	size=34
  foo/dir1/file3	07429fb6fd938b5753b26d043f7fca053e437eab71cda1fea527503d1251f103	type=regular	size=45
  foo/dir1/file4	983aa47751a102ff612e7a4c22334f55ca7b096018af300de31a2bc0a354e769	type=regular	size=3
  foo/file2	a04945366726ef3cec5979bc209418349dcc55450c4fbdbf72c7df9a6d291eab	type=regular	size=9
  qu/ux/dir1/file3	ab4cfaaae9130d5fe0da336c11e9bbb1548999e220d7f5b8a2141b492497a1bd	type=regular	size=76
  qu/ux/dir1/file4	bb3283f3abf1288e1daae489d17bc43577f2fc62726c322d75b6cd062ec5a6c5	type=regular	size=12
  qu/ux/file2	a04945366726ef3cec5979bc209418349dcc55450c4fbdbf72c7df9a6d291eab	type=regular	size=9

  $ mononoke_admin derived-data -R repo verify-manifests -i $K \
  >     -T hgchangesets -T unodes -T skeleton_manifests \
  >     -T content_manifests -T git_commits

  $ start_and_wait_for_scs_server

  $ scsc log -R repo -i $I -p foo | grep ^Summary:
  Summary: E
  Summary: C
  Summary: B
  Summary: A

  $ scsc log -R repo -i $I -p bar | grep ^Summary:
  Summary: H
  Summary: G
  Summary: E
  Summary: Subtree copy from 93b5138860f0eaaf84498fe9cf9a5fcf710edc91

  $ scsc log -R repo -i $I -p baz | grep ^Summary:
  Summary: I
  Summary: Subtree copy from 18dc0fef93d410fa82d06640fdf7cb4c3b14d903

  $ scsc blame --title-width=13 -TOPlon -R repo -i $I -p baz/dir1/file3
  #0 1c58c52a6ce266ec070a11510322594e5018d611:A            :   :foo/dir1/file3:1:1: a3 (i)
  #1 93b5138860f0eaaf84498fe9cf9a5fcf710edc91:B            :2-2:foo/dir1/file3:2:2: b3 (ii)
  #0 1c58c52a6ce266ec070a11510322594e5018d611:A            :   :foo/dir1/file3:3:3: a3 (iii)
  #5 fbb84feb4df70a07a4598f21d8bb31c0d250c278:I            : +4:              :4:4: i3 (merge) {baz}
  #1 93b5138860f0eaaf84498fe9cf9a5fcf710edc91:B            : +4:foo/dir1/file3:4:5: b3 (iv)
  #2 18dc0fef93d410fa82d06640fdf7cb4c3b14d903:E            : +5:foo/dir1/file3:5:6: e3 (v) {foo}
  #4 d883cb4b3b1cf4cf5ea33241e98b78b6ee2b5fbd:G            : +5:bar/dir1/file3:5:7: g3 (vi) {bar}


  $ scsc blame --title-width=13 -TOPlon -R repo -i $I -p baz/dir1/file4
  #0 1c58c52a6ce266ec070a11510322594e5018d611:A            :                                                            :foo/dir1/file4:1:1: a4
  #1 c8d50c74f28be60593b6e7d43230cfab2fc10085:Subtree copy :(18dc0fef93d410fa82d06640fdf7cb4c3b14d903)[foo/dir1/file4]+2:              :2:2: f4 {baz}

  $ scsc blame --title-width=13 -TOPlon -R repo -i $I -p bar/file2
  #0 1c58c52a6ce266ec070a11510322594e5018d611:A            :             :foo/file1:1:1: a1
  #1 93b5138860f0eaaf84498fe9cf9a5fcf710edc91:B            :[foo/file1]+2:foo/file2:2:2: b2
  #3 18dc0fef93d410fa82d06640fdf7cb4c3b14d903:E            :           +3:         :3:3: e2 {bar}

  $ scsc blame --title-width=13 -TOPlon -R repo -i $I -p foo/file2
  #0 1c58c52a6ce266ec070a11510322594e5018d611:A            :             :foo/file1:1:1: a1
  #1 93b5138860f0eaaf84498fe9cf9a5fcf710edc91:B            :[foo/file1]+2:         :2:2: b2
  #2 d29591b3c81c3685900bcf7023dd184657d897e4:C            :           +3:         :3:3: c2

  $ scsc blame --title-width=13 -Tln -R repo -i $I -p baz/fileX
  #0 fbb84feb4df70a07a4598f21d8bb31c0d250c278:I            :1: additional file

  $ scsc find-files -R repo -i $I | sort
  bar/dir1/file3
  bar/dir1/file4
  bar/file2
  baz/dir1/file3
  baz/dir1/file4
  baz/file2
  baz/fileX
  foo/dir1/file3
  foo/dir1/file4
  foo/file2

  $ scsc find-files -R repo -i $I --suffix 3 | sort
  bar/dir1/file3
  baz/dir1/file3
  foo/dir1/file3

  $ scsc ls -R repo -l -i $I -p baz
  tree        88  dir1/
  file         9  file2
  file        16  fileX

Comparing the commit directly with its parent shows all files added
  $ scsc diff --ordered -R repo -i $D
  diff --git a/bar/dir1/file3 b/bar/dir1/file3
  new file mode 100644
  --- /dev/null
  +++ b/bar/dir1/file3
  @@ -0,0 +1,4 @@
  +a3 (i)
  +b3 (ii)
  +a3 (iii)
  +b3 (iv)
  diff --git a/bar/dir1/file4 b/bar/dir1/file4
  new file mode 100644
  --- /dev/null
  +++ b/bar/dir1/file4
  @@ -0,0 +1,1 @@
  +a4
  diff --git a/bar/file2 b/bar/file2
  new file mode 100644
  --- /dev/null
  +++ b/bar/file2
  @@ -0,0 +1,2 @@
  +a1
  +b2

Comparing a clean subtree copy with the copy source shows no changes
  $ scsc diff -R repo -i $D --compare-with-subtree-copy-sources
  subtree copy 93b5138860f0eaaf84498fe9cf9a5fcf710edc91 foo bar

Comparing a copy with an additional change with the copy source shows that additional change
  $ scsc diff -R repo -i $F --compare-with-subtree-copy-sources
  subtree copy 18dc0fef93d410fa82d06640fdf7cb4c3b14d903 foo baz
  diff --git a/baz/dir1/file4 b/baz/dir1/file4
  --- a/baz/dir1/file4
  +++ b/baz/dir1/file4
  @@ -1,1 +1,2 @@
   a4
  +f4 {baz}

Same for --ordered
  $ scsc diff --ordered -R repo -i $F --compare-with-subtree-copy-sources
  subtree copy 18dc0fef93d410fa82d06640fdf7cb4c3b14d903 foo baz
  diff --git a/baz/dir1/file4 b/baz/dir1/file4
  --- a/baz/dir1/file4
  +++ b/baz/dir1/file4
  @@ -1,1 +1,2 @@
   a4
  +f4 {baz}

And for metadata diffs
  $ scsc diff -R repo -i $F --compare-with-subtree-copy-sources -f metadata-diff
  subtree copy 18dc0fef93d410fa82d06640fdf7cb4c3b14d903 foo baz
  --- a/foo/dir1/file4, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  +++ b/baz/dir1/file4, file type: FILE, content type: TEXT, generated status: NOT_GENERATED,
  first added line number: 2, 1 significant lines (1 added, 0 deleted), 1 total (1 added, 0 deleted)
  
Comparing a subtree merge is the same as comparing with the parent (the merge source is always the second parent)
  $ scsc diff --ordered -R repo -i $I --compare-with-subtree-copy-sources
  subtree merge d883cb4b3b1cf4cf5ea33241e98b78b6ee2b5fbd bar baz
  diff --git a/baz/dir1/file3 b/baz/dir1/file3
  --- a/baz/dir1/file3
  +++ b/baz/dir1/file3
  @@ -1,5 +1,7 @@
   a3 (i)
   b3 (ii)
   a3 (iii)
  +i3 (merge) {baz}
   b3 (iv)
   e3 (v) {foo}
  +g3 (vi) {bar}
  diff --git a/baz/fileX b/baz/fileX
  new file mode 100644
  --- /dev/null
  +++ b/baz/fileX
  @@ -0,0 +1,1 @@
  +additional file

Comparing a subtree copy that also deleted a file shows the file being deleted
  $ scsc diff -S bonsai,hg -R repo -i $K --compare-with-subtree-copy-sources
  subtree copy bonsai=738339183bb534d7335602c081b6c153259027118e35fc6558a49ad990b0606c,hg=fbb84feb4df70a07a4598f21d8bb31c0d250c278 baz qu/ux
  diff --git a/qu/ux/fileX b/qu/ux/fileX
  deleted file mode 100644
  --- a/qu/ux/fileX
  +++ /dev/null
  @@ -1,1 +0,0 @@
  -additional file

Testing ordered diff with paths-only for subtree replacements
  $ scsc diff --ordered --paths-only -R repo -i $F --compare-with-subtree-copy-sources
  subtree copy 18dc0fef93d410fa82d06640fdf7cb4c3b14d903 foo baz
  M baz/dir1/file4

  $ scsc diff --ordered --paths-only -R repo -i $I --compare-with-subtree-copy-sources
  subtree merge d883cb4b3b1cf4cf5ea33241e98b78b6ee2b5fbd bar baz
  M baz/dir1/file3
  A baz/fileX

  $ scsc diff --ordered --paths-only -R repo -i $K --compare-with-subtree-copy-sources
  subtree copy fbb84feb4df70a07a4598f21d8bb31c0d250c278 baz qu/ux
  D qu/ux/fileX

Compact restrictions cover both parent and source-relative changed paths.

  $ scsc restricted-paths changes -R repo -i $K --page-size 1 --json > subtree-restrictions.json
  $ jq -c '.changed_paths | index("qu/ux/fileX") != null' subtree-restrictions.json
  true
  $ scsc diff --ordered --paths-only -R repo -i $K > parent-paths.txt
  $ scsc diff --ordered --paths-only -R repo -i $K --compare-with-subtree-copy-sources > source-paths.txt
  $ python3 - <<'PY'
  > import json
  > def paths(file):
  >     return {line[2:].strip() for line in open(file) if line.startswith(('A ', 'M ', 'D '))}
  > actual = set(json.load(open('subtree-restrictions.json'))['changed_paths'])
  > assert actual == paths('parent-paths.txt') | paths('source-paths.txt')
  > PY

Testing cross-repo subtree operations (subtree import and cross-repo merge)
  $ scsc diff --ordered -S bonsai,hg -R repo -i $L --compare-with-subtree-copy-sources
  subtree import git+file://$TESTTMP/gitrepo d3bd984c3a882240b7b1b06e6debd5e564ba3c58  xgitrepo
  diff --git a/xgitrepo/alpha b/xgitrepo/alpha
  new file mode 100644
  --- /dev/null
  +++ b/xgitrepo/alpha
  @@ -0,0 +1,1 @@
  +1
  diff --git a/xgitrepo/dir1/beta b/xgitrepo/dir1/beta
  new file mode 100644
  --- /dev/null
  +++ b/xgitrepo/dir1/beta
  @@ -0,0 +1,1 @@
  +b1
  $ scsc diff -S bonsai,hg -R repo -i $M --compare-with-subtree-copy-sources
  subtree cross_repo_merge file://$TESTTMP/gitrepo 0b468b5eb6b8ea3401c5bb6fafbd190ed01506e2  xgitrepo
  diff --git a/xgitrepo/dir1/beta b/xgitrepo/dir1/beta
  --- a/xgitrepo/dir1/beta
  +++ b/xgitrepo/dir1/beta
  @@ -1,1 +1,1 @@
  -b1
  +b2
