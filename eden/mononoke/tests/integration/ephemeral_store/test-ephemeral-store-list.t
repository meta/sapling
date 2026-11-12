# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ . "${TEST_FIXTURES}/library-snapshot.sh"

setup configuration
  $ base_snapshot_repo_setup client1
  A=20ca2a4749a439b459125ef0f6a4f26e88ee7538
  $ cd client1
  $ mkdir test_tmp
  $ cd test_tmp
  $ echo "a file content" > a
  $ echo "b file content" > b
  $ hg add b
  $ echo "c file content" > c
  $ echo "d file content" > d
  $ echo "e file content" > e
  $ echo "f file content" > f
  $ hg add f
  $ hg snapshot create
  snapshot: Snapshot created with id 91dba37ed314260bd1d66c76a3cd5623d8edb553401cb5e69db644447efede07
  0 files modified, 2 files added, 0 files removed, 0 files missing, 4 files untracked

List the blob contents of a bubble without passing any argument:
  $ mononoke_admin ephemeral-store -R repo list
  Error: Need to provide either changeset ID or bubble ID as input
  [1]
List the blob contents of a bubble using the bubble ID:
  $ mononoke_admin ephemeral-store -R repo list -b 1 --ordered
  eph1.repo0000.alias.gitsha1.6b9963d7b81521bc09655857e272bf0f130e6bc3
  eph1.repo0000.alias.gitsha1.6fd97149d33202f1315d756e358078299c30bd7c
  eph1.repo0000.alias.gitsha1.904cb6675fad760f9fa1c27385e0c0c3d102820e
  eph1.repo0000.alias.gitsha1.a8cd0e934a12ed0f4f98673c6823c3a01f8260ff
  eph1.repo0000.alias.gitsha1.ab239e6bda2873d35753ed5267a70c49799fb465
  eph1.repo0000.alias.gitsha1.c1c0679bb56e42afff11f124d24d33b5c0fdb444
  eph1.repo0000.alias.seeded_blake3.33715dbb3ac75d052bd385d2f5be43f70dc455c9849b2ee6a618fbac43d71a4f
  eph1.repo0000.alias.seeded_blake3.50534506fccbd888d446a3d25e94500fae3039efccb9cfa5d7ae49b222d9475a
  eph1.repo0000.alias.seeded_blake3.ab1ef9899bcad454f171457b4396523e8d96f4177721124b8e0588f66c36e989
  eph1.repo0000.alias.seeded_blake3.ac2495b5eb93c5f6ec5730282316ee116d0bb154860b50cbd0a2dc4bfabbba7c
  eph1.repo0000.alias.seeded_blake3.d2a8f8fc8d8b9fa2666d606480d211f69539dcf06ebe33c7f6e0170e9f998952
  eph1.repo0000.alias.seeded_blake3.e201855acf04fcf499ed30d38b249afd2719b1a4d0580066a48c851d598ea019
  eph1.repo0000.alias.sha1.08e1c92c5f2ff43e14145e68c0842d47ce9b7ef4
  eph1.repo0000.alias.sha1.0dfa1747665759ba2c865cbd5b7a7925c4389148
  eph1.repo0000.alias.sha1.3c655550bedc3add76f50b943e3c19f5ffc364de
  eph1.repo0000.alias.sha1.4f5c253b3ebc5899a9d1544249ceae4b64507ec9
  eph1.repo0000.alias.sha1.8e24abef07055b6a3dd55b81aa313d8ffa068890
  eph1.repo0000.alias.sha1.ff23b0304b0cf5533b8b5ad9e6ce97b3794695c4
  eph1.repo0000.alias.sha256.264ff4b68a0cf01da34ad37e375334d3298f57a22cf9cbe225c23483358ffa7a
  eph1.repo0000.alias.sha256.6c897d2340364ed22c9a3dce7f66b2399126d1318dca38025cae93a59d574fb9
  eph1.repo0000.alias.sha256.7c954461b519817c2ea8941450e283e6f757d08effb3b2c7645b9fcaacf2b2a8
  eph1.repo0000.alias.sha256.cb3061efdc399df099de7deadc56ce10e3512a9e937e27912b2afbf4af3c4f1e
  eph1.repo0000.alias.sha256.d1fcf04a9fcbae8bc5941649b4c9b5214116619075db4cdc7922e0687b155007
  eph1.repo0000.alias.sha256.da566161af52cc24d05681472324f280da04e51be8b6b9466ae2032a27c52f96
  eph1.repo0000.changeset.blake2.91dba37ed314260bd1d66c76a3cd5623d8edb553401cb5e69db644447efede07
  eph1.repo0000.content.blake2.4f3fc85925a86f48ba4052a20c4d70ac9c8024f4e2d984870f5a292ffb701f4d
  eph1.repo0000.content.blake2.6b0f000404b62473b82f51e1faa119c2ed7652e03188bf2770b0f701cae5c699
  eph1.repo0000.content.blake2.74561488c4d96fb423fa43522623d710eb4cad120d5d63565ecdab5e9c2d5dc2
  eph1.repo0000.content.blake2.809a236c1e76ef09440ad7c06577ebd68f67186882862c2265e7481aea96af92
  eph1.repo0000.content.blake2.8ff72c730b5cb84ca1d9f0ed64427af818f7a7e197d38c2da9e813b8b430cbac
  eph1.repo0000.content.blake2.d39ff8be35d80756c6c65a40b8c4d1e7c64f04ff6f99d77d2fadda34cb3dc6b1
  eph1.repo0000.content_metadata2.blake2.4f3fc85925a86f48ba4052a20c4d70ac9c8024f4e2d984870f5a292ffb701f4d
  eph1.repo0000.content_metadata2.blake2.6b0f000404b62473b82f51e1faa119c2ed7652e03188bf2770b0f701cae5c699
  eph1.repo0000.content_metadata2.blake2.74561488c4d96fb423fa43522623d710eb4cad120d5d63565ecdab5e9c2d5dc2
  eph1.repo0000.content_metadata2.blake2.809a236c1e76ef09440ad7c06577ebd68f67186882862c2265e7481aea96af92
  eph1.repo0000.content_metadata2.blake2.8ff72c730b5cb84ca1d9f0ed64427af818f7a7e197d38c2da9e813b8b430cbac
  eph1.repo0000.content_metadata2.blake2.d39ff8be35d80756c6c65a40b8c4d1e7c64f04ff6f99d77d2fadda34cb3dc6b1

List the blob contents of a bubble using invalid bubble ID:
  $ mononoke_admin ephemeral-store -R repo list -b 100001
  Error: bubble 100001 does not exist, or has expired
  [1]
List the blob contents of a bubble using the changeset ID:
  $ mononoke_admin ephemeral-store -R repo list -i 91dba37ed314260bd1d66c76a3cd5623d8edb553401cb5e69db644447efede07 --ordered
  eph1.repo0000.alias.gitsha1.6b9963d7b81521bc09655857e272bf0f130e6bc3
  eph1.repo0000.alias.gitsha1.6fd97149d33202f1315d756e358078299c30bd7c
  eph1.repo0000.alias.gitsha1.904cb6675fad760f9fa1c27385e0c0c3d102820e
  eph1.repo0000.alias.gitsha1.a8cd0e934a12ed0f4f98673c6823c3a01f8260ff
  eph1.repo0000.alias.gitsha1.ab239e6bda2873d35753ed5267a70c49799fb465
  eph1.repo0000.alias.gitsha1.c1c0679bb56e42afff11f124d24d33b5c0fdb444
  eph1.repo0000.alias.seeded_blake3.33715dbb3ac75d052bd385d2f5be43f70dc455c9849b2ee6a618fbac43d71a4f
  eph1.repo0000.alias.seeded_blake3.50534506fccbd888d446a3d25e94500fae3039efccb9cfa5d7ae49b222d9475a
  eph1.repo0000.alias.seeded_blake3.ab1ef9899bcad454f171457b4396523e8d96f4177721124b8e0588f66c36e989
  eph1.repo0000.alias.seeded_blake3.ac2495b5eb93c5f6ec5730282316ee116d0bb154860b50cbd0a2dc4bfabbba7c
  eph1.repo0000.alias.seeded_blake3.d2a8f8fc8d8b9fa2666d606480d211f69539dcf06ebe33c7f6e0170e9f998952
  eph1.repo0000.alias.seeded_blake3.e201855acf04fcf499ed30d38b249afd2719b1a4d0580066a48c851d598ea019
  eph1.repo0000.alias.sha1.08e1c92c5f2ff43e14145e68c0842d47ce9b7ef4
  eph1.repo0000.alias.sha1.0dfa1747665759ba2c865cbd5b7a7925c4389148
  eph1.repo0000.alias.sha1.3c655550bedc3add76f50b943e3c19f5ffc364de
  eph1.repo0000.alias.sha1.4f5c253b3ebc5899a9d1544249ceae4b64507ec9
  eph1.repo0000.alias.sha1.8e24abef07055b6a3dd55b81aa313d8ffa068890
  eph1.repo0000.alias.sha1.ff23b0304b0cf5533b8b5ad9e6ce97b3794695c4
  eph1.repo0000.alias.sha256.264ff4b68a0cf01da34ad37e375334d3298f57a22cf9cbe225c23483358ffa7a
  eph1.repo0000.alias.sha256.6c897d2340364ed22c9a3dce7f66b2399126d1318dca38025cae93a59d574fb9
  eph1.repo0000.alias.sha256.7c954461b519817c2ea8941450e283e6f757d08effb3b2c7645b9fcaacf2b2a8
  eph1.repo0000.alias.sha256.cb3061efdc399df099de7deadc56ce10e3512a9e937e27912b2afbf4af3c4f1e
  eph1.repo0000.alias.sha256.d1fcf04a9fcbae8bc5941649b4c9b5214116619075db4cdc7922e0687b155007
  eph1.repo0000.alias.sha256.da566161af52cc24d05681472324f280da04e51be8b6b9466ae2032a27c52f96
  eph1.repo0000.changeset.blake2.91dba37ed314260bd1d66c76a3cd5623d8edb553401cb5e69db644447efede07
  eph1.repo0000.content.blake2.4f3fc85925a86f48ba4052a20c4d70ac9c8024f4e2d984870f5a292ffb701f4d
  eph1.repo0000.content.blake2.6b0f000404b62473b82f51e1faa119c2ed7652e03188bf2770b0f701cae5c699
  eph1.repo0000.content.blake2.74561488c4d96fb423fa43522623d710eb4cad120d5d63565ecdab5e9c2d5dc2
  eph1.repo0000.content.blake2.809a236c1e76ef09440ad7c06577ebd68f67186882862c2265e7481aea96af92
  eph1.repo0000.content.blake2.8ff72c730b5cb84ca1d9f0ed64427af818f7a7e197d38c2da9e813b8b430cbac
  eph1.repo0000.content.blake2.d39ff8be35d80756c6c65a40b8c4d1e7c64f04ff6f99d77d2fadda34cb3dc6b1
  eph1.repo0000.content_metadata2.blake2.4f3fc85925a86f48ba4052a20c4d70ac9c8024f4e2d984870f5a292ffb701f4d
  eph1.repo0000.content_metadata2.blake2.6b0f000404b62473b82f51e1faa119c2ed7652e03188bf2770b0f701cae5c699
  eph1.repo0000.content_metadata2.blake2.74561488c4d96fb423fa43522623d710eb4cad120d5d63565ecdab5e9c2d5dc2
  eph1.repo0000.content_metadata2.blake2.809a236c1e76ef09440ad7c06577ebd68f67186882862c2265e7481aea96af92
  eph1.repo0000.content_metadata2.blake2.8ff72c730b5cb84ca1d9f0ed64427af818f7a7e197d38c2da9e813b8b430cbac
  eph1.repo0000.content_metadata2.blake2.d39ff8be35d80756c6c65a40b8c4d1e7c64f04ff6f99d77d2fadda34cb3dc6b1


List the blob contents of a bubble using invalid changeset ID:
  $ mononoke_admin ephemeral-store -R repo list -i ofcourse_this_is_invalid
  Error: invalid blake2 input: need exactly 64 hex digits
  [1]

List the blob contents of a bubble using non-matching changeset ID:
  $ mononoke_admin ephemeral-store -R repo list -i 39c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5
  Error: No bubble exists for changeset ID 39c49a9ad363e4a2f0c314093683a84a85bfaa7b4da83046e58ccb4fbeb2f6c5
  [1]

List the blob contents of a bubble after limiting the results:
  $ mononoke_admin ephemeral-store -R repo list -b 1 -l 4 --start-from repo0000.content_metadata.blake2 --ordered
  eph1.repo0000.content_metadata2.blake2.* (glob)
  eph1.repo0000.content_metadata2.blake2.* (glob)
  eph1.repo0000.content_metadata2.blake2.* (glob)
  eph1.repo0000.content_metadata2.blake2.* (glob)
List the blob contents of a bubble after a specified key:
  $ mononoke_admin ephemeral-store -R repo list -b 1 --start-from repo0000.content_metadata --ordered
  eph1.repo0000.content_metadata2.blake2.4f3fc85925a86f48ba4052a20c4d70ac9c8024f4e2d984870f5a292ffb701f4d
  eph1.repo0000.content_metadata2.blake2.6b0f000404b62473b82f51e1faa119c2ed7652e03188bf2770b0f701cae5c699
  eph1.repo0000.content_metadata2.blake2.74561488c4d96fb423fa43522623d710eb4cad120d5d63565ecdab5e9c2d5dc2
  eph1.repo0000.content_metadata2.blake2.809a236c1e76ef09440ad7c06577ebd68f67186882862c2265e7481aea96af92
  eph1.repo0000.content_metadata2.blake2.8ff72c730b5cb84ca1d9f0ed64427af818f7a7e197d38c2da9e813b8b430cbac
  eph1.repo0000.content_metadata2.blake2.d39ff8be35d80756c6c65a40b8c4d1e7c64f04ff6f99d77d2fadda34cb3dc6b1
