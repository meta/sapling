# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
Setup repo
  $ REPONAME="test/repo"
  $ setup_common_config
  $ testtool_drawdag --repo-name test/repo --no-default-files << EOF
  > A-B-C-D-E-F-G-H-I-J
  >      \     /
  >       K-L-M
  > # modify: B dir/file content
  > # modify: B dir/other_file other_content
  > # modify: D dir/file new_content
  > # modify: G foobar contentMerged
  > # modify: G dir/file contentMerged
  > # modify: F foobar contentF
  > # modify: M foobar contentM
  > EOF
  A=1b67a29aa79a804c85c94f1bef677daddf199deca00394423b2e8efef5efe6a8
  B=d4b850d7c7caa9ae5a3da7aa458cd485ae4f2a1646f29e2d851473b2b2bab4d9
  C=ef247a9028417ef3bd6f5000f910ffd19e942a616b3fe758213fc2e7ec1e58e5
  D=dcb785b1b2acaf8460050539ce360a7e7fa2dbfb8f7936ca14d04a2fd380f61f
  E=40c8635aa97598d59a2098785249900241928557fc151643d1617207812cec72
  F=18489a333d33163376fe0ed2b65a9cb6382ad7a4181c27dd4964d1e2c77554cb
  G=60f6b0d08d5e7264f450530f38def12d5ae2a77edc79c25c4af8bfa04c67a43f
  H=f0bfeddc4966faec79ea1fb10873cef7c69f3fb60735ec80af42ffdde95bd3f2
  I=4b439631f22ae802957445743f34bd0a2fc5ed6f8e6e223b95c77ca45e9fb04a
  J=82d360b1e71363b9c193ec5553cf66e3acff064e4c7ff7f39652d7102fa32073
  K=5f7cfea3bcc3daa9e7dddc51b2a9dd198c95e3d256ab2642b8967665e04a21e9
  L=b51e327e5309b989a9b2cd5322203eb393d8a709318b38c0e3cddd277c89cbf0
  M=26690b2999b95d01d6e98808f51ae87de11c3628eb3b7aa43ad026712b3a65fb

  $ start_and_wait_for_scs_server
  $ export SCSC_WRITES_ENABLED=true


Trying to derive B and C, but A was not derived yet and they are not in topological order
It should work and while not even A was derived first, every commit up to C is derived at the end
  $ mononoke_admin derived-data -R test/repo exists -T fsnodes -i "$A"
  Not Derived: 1b67a29aa79a804c85c94f1bef677daddf199deca00394423b2e8efef5efe6a8
  $ scsc prepare-commits -R test/repo --bonsai-id $C --bonsai-id $B --derived-data-type FSNODE
  $ mononoke_admin derived-data -R test/repo exists -T fsnodes -i "$C" -i "$D" -i "$K"
  Derived: ef247a9028417ef3bd6f5000f910ffd19e942a616b3fe758213fc2e7ec1e58e5
  Not Derived: dcb785b1b2acaf8460050539ce360a7e7fa2dbfb8f7936ca14d04a2fd380f61f
  Not Derived: 5f7cfea3bcc3daa9e7dddc51b2a9dd198c95e3d256ab2642b8967665e04a21e9

Trying to derive the entire graph at once by asking for J should also work
Note that deleted_manifests depends on unodes, so that will be derived too
  $ mononoke_admin derived-data -R test/repo exists -T unodes -i "$A"
  Not Derived: 1b67a29aa79a804c85c94f1bef677daddf199deca00394423b2e8efef5efe6a8
  $ mononoke_admin derived-data -R test/repo exists -T deleted_manifest -i "$A"
  Not Derived: 1b67a29aa79a804c85c94f1bef677daddf199deca00394423b2e8efef5efe6a8
  $ scsc prepare-commits -R test/repo --bonsai-id $J --derived-data-type DELETED_MANIFEST_V2
  $ mononoke_admin derived-data -R test/repo exists -T unodes -i "$J"
  Derived: 82d360b1e71363b9c193ec5553cf66e3acff064e4c7ff7f39652d7102fa32073
  $ mononoke_admin derived-data -R test/repo exists -T deleted_manifest -i "$J"
  Derived: 82d360b1e71363b9c193ec5553cf66e3acff064e4c7ff7f39652d7102fa32073
