# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ GIT_REPO="${TESTTMP}/repo-git"
  $ HG_REPO="${TESTTMP}/repo"
  $ setup_common_config blob_files
  $ setconfig remotenames.selectivepulldefault=master_bookmark,branch1,branch2

# Setup git repsitory
  $ mkdir "$GIT_REPO"
  $ cd "$GIT_REPO"
  $ git init -q
  $ git commit --allow-empty -m "root commit"
  [master_bookmark (root-commit) d53a2ef] root commit
  $ git branch root

  $ echo "this is master" > master
  $ git add master
  $ git commit -qam "Add master"

  $ git checkout -q root
  $ git checkout -qb branch1
  $ echo "this is branch1" > branch1
  $ git add branch1
  $ git commit -qam "Add branch1"

  $ git checkout -q root
  $ git checkout -qb branch2
  $ echo "this is branch2" > branch2
  $ git add branch2
  $ git commit -qam "Add branch2"

  $ git checkout -q master_bookmark
  $ git merge branch1 branch2
  Trying simple merge with branch1
  Trying simple merge with branch2
  Merge made by the 'octopus' strategy.
   branch1 | 1 +
   branch2 | 1 +
   2 files changed, 2 insertions(+)
   create mode 100644 branch1
   create mode 100644 branch2

# Import it into Mononoke
  $ cd "$TESTTMP"
  $ gitimport "$GIT_REPO" full-repo
  [INFO] using repo "repo" repoid RepositoryId(0)
  [INFO] GitRepo:$TESTTMP/repo-git commit 5 of 5 - Oid:207fa6e3 => Bid:cc6482da
  [INFO] Ref: "refs/heads/branch1": Some(ChangesetId(Blake2(7db05defca4d86fbafe97460d971c74fcb571da08f921252990831d86816ab5f)))
  [INFO] Ref: "refs/heads/branch2": Some(ChangesetId(Blake2(3f6085be18c9325ed283c4b4f766737470a243e00f7236db58e48ab918410d45)))
  [INFO] Ref: "refs/heads/master_bookmark": Some(ChangesetId(Blake2(cc6482da2d0514725c8e52d064f83963dfbc3b4f7d6bee8e90cc21e2dd555f7d)))
  [INFO] Ref: "refs/heads/root": Some(ChangesetId(Blake2(3127796ac597cbfe000475927080d809512a797d800cb0af0153d37533544ef3)))

# Validate if creating the commits also uploaded the raw commit blobs AND the raw tree blobs
# The id of the blobs should be the same as the commit and tree object ids
  $ ls $TESTTMP/blobstore/blobs | grep "git_object"
  blob-repo0000.git_object.161a8cb720352af550786d4e73eeb36d5b958ddd
  blob-repo0000.git_object.207fa6e38ee349424b04123391424f128cec6bcf
  blob-repo0000.git_object.345b79b8bd294b77d50384ffa777c56191620334
  blob-repo0000.git_object.3e11aab71d3e96c63139b4f68c0f0c65e86e078c
  blob-repo0000.git_object.4b825dc642cb6eb9a060e54bf8d69288fbee4904
  blob-repo0000.git_object.8b275500af68d631c2062eb45c743174aaadf224
  blob-repo0000.git_object.933c6d8556a071c2105b8b2fd1dabff709d87929
  blob-repo0000.git_object.a6fb918088a115d0f76618a4d048339cd2abcf69
  blob-repo0000.git_object.bf946c828dea5fe0a0228dc7d556aa4a524df2d1
  blob-repo0000.git_object.d53a2ef2bbadbe26f8c28598b408e03c0b01027c

# Set master_bookmark (gitimport does not do this yet)
  $ mononoke_admin bookmarks -R repo set master_bookmark cc6482da2d0514725c8e52d064f83963dfbc3b4f7d6bee8e90cc21e2dd555f7d
  Creating publishing bookmark master_bookmark at cc6482da2d0514725c8e52d064f83963dfbc3b4f7d6bee8e90cc21e2dd555f7d

# Start Mononoke
  $ start_and_wait_for_mononoke_server
# Clone the repository
  $ cd "$TESTTMP"
  $ hg clone -q mono:repo "$HG_REPO"
  $ cd "$HG_REPO"
  $ tail master branch1 branch2
  ==> master <==
  this is master
  * (glob)
  ==> branch1 <==
  this is branch1
  * (glob)
  ==> branch2 <==
  this is branch2

# Check the extras
  $ hg log -r . -T '{extras % "{extra}\n"}'
  branch=default
  convert_revision=207fa6e38ee349424b04123391424f128cec6bcf
  hg-git-rename-source=git
  stepparents=2cc8d8df26cc8965cda5ff2aef95fc67d4a6aae2

# Check the filenodes
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT HEX(filenode), HEX(linknode) FROM filenodes ORDER BY filenode DESC;"
  DDAE7A95B6B0FB27DFACC4051C41AA9CFF30C1E2|3E11F5E9E3E90C064F0AF238475FC6BEDD9527B9
  DB9F6E90B4D31605949C7E5273E72FEADE04E6C4|2CC8D8DF26CC8965CDA5FF2AEF95FC67D4A6AAE2
  D5E651FDE2FF4278E3172BF3BEDACCAE9F466C89|0A093A76F75C2982CF237E1F1F2119D605E9187B
  B80DE5D138758541C5F05265AD144AB9FA86D1DB|BEF3DCB7B15F0EF70320072A22AA851993B12DA1
  B24D823C90409CA8CE2AC2BB22DAD5C6B9D17D4D|2CC8D8DF26CC8965CDA5FF2AEF95FC67D4A6AAE2
  8D8AC2F4A8AF10BA885E164A5F33CB4F91F8A0F8|0A093A76F75C2982CF237E1F1F2119D605E9187B
  290DD67AD15DE1444C88A016BE6EC55CDF056C10|3E11F5E9E3E90C064F0AF238475FC6BEDD9527B9
  1A4ECD744147A79966E5473A3B86B447533ABF9D|20572A577CFEAF2580444148D6ABD34121A0899B
