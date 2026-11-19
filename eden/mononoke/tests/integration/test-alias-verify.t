# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ CACHEDIR=$PWD/cachepath
  $ . "${TEST_FIXTURES}/library.sh"

# setup config repo

  $ REPOTYPE="blob_files"
  $ setup_common_config $REPOTYPE
  $ cd $TESTTMP

  $ quiet testtool_drawdag -R repo << EOF
  > C
  > |
  > B
  > |
  > A
  > # bookmark: C master_bookmark
  > EOF


  $ ls $TESTTMP/blobstore/blobs | grep "alias"
  blob-repo0000.alias.gitsha1.7371f47a6f8bd23a8fa1a8b2a9479cdd76380e54
  blob-repo0000.alias.gitsha1.8c7e5a667f1b771847fe88c01c3de34413a1b220
  blob-repo0000.alias.gitsha1.96d80cd6c4e7158dbebd0849f4fb7ce513e5828c
  blob-repo0000.alias.seeded_blake3.5667f2421ac250c4bb9af657b5ead3cdbd940bfbc350b2bfee47454643832b48
  blob-repo0000.alias.seeded_blake3.5ad3ba58a716e5fc04296ac9af7a1420f726b401fdf16d270beb5b6b30bc0cda
  blob-repo0000.alias.seeded_blake3.6fb4c384e79ac0771a483fcf3c46fb4ea8609f79608e8bcbf710f9887a3b9cf6
  blob-repo0000.alias.sha1.32096c2e0eff33d844ee6d675407ace18289357d
  blob-repo0000.alias.sha1.6dcd4ce23d88e2ee9568ba546c007c63d9131c1b
  blob-repo0000.alias.sha1.ae4f281df5a5d0ff3cad6371f76d5c29b6d953ec
  blob-repo0000.alias.sha256.559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd
  blob-repo0000.alias.sha256.6b23c0d5f35d1b11f9b683f0b0a617355deb11277d91ae091d399c655b87940d
  blob-repo0000.alias.sha256.df7e70e5021544f4834bbee64a9e3789febc4be81470df629cad6ddb03320a5c

#Alias verification with Sha256 alias
  $ aliasverify verify sha256 2>&1 | grep "Alias Verification"
  [INFO] Alias Verification continues: 0 errors found
  [INFO] Alias Verification finished: 0 errors found
#Alias verification with SeededBlake3 alias
  $ aliasverify verify seeded-blake3 2>&1 | grep "Alias Verification"
  [INFO] Alias Verification continues: 0 errors found
  [INFO] Alias Verification finished: 0 errors found
#Alias verification with Sha1 alias
  $ aliasverify verify sha1 2>&1 | grep "Alias Verification"
  [INFO] Alias Verification continues: 0 errors found
  [INFO] Alias Verification finished: 0 errors found
#Alias verification with GitSha1 alias
  $ aliasverify verify git-sha1 2>&1 | grep "Alias Verification"
  [INFO] Alias Verification continues: 0 errors found
  [INFO] Alias Verification finished: 0 errors found

  $ rm -rf $TESTTMP/blobstore/blobs/blob-repo0000.alias.*
  $ ls $TESTTMP/blobstore/blobs | grep "alias" | wc -l
  0

  $ aliasverify verify sha256 2>&1 | grep "Alias Verification"
  [INFO] Alias Verification continues: 3 errors found
  [INFO] Alias Verification finished: 3 errors found

#Missing Sha256 aliases
  $ aliasverify verify sha256 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias Sha256(559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias Sha256(6b23c0d5f35d1b11f9b683f0b0a617355deb11277d91ae091d399c655b87940d), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))
  [DEBUG] Missing alias blob: alias Sha256(df7e70e5021544f4834bbee64a9e3789febc4be81470df629cad6ddb03320a5c), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
#Missing SeededBlake3 aliases
  $ aliasverify verify seeded-blake3 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias Blake3(5667f2421ac250c4bb9af657b5ead3cdbd940bfbc350b2bfee47454643832b48), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
  [DEBUG] Missing alias blob: alias Blake3(5ad3ba58a716e5fc04296ac9af7a1420f726b401fdf16d270beb5b6b30bc0cda), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias Blake3(6fb4c384e79ac0771a483fcf3c46fb4ea8609f79608e8bcbf710f9887a3b9cf6), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))
#Missing Sha1 aliases
  $ aliasverify verify sha1 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias Sha1(32096c2e0eff33d844ee6d675407ace18289357d), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))
  [DEBUG] Missing alias blob: alias Sha1(6dcd4ce23d88e2ee9568ba546c007c63d9131c1b), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias Sha1(ae4f281df5a5d0ff3cad6371f76d5c29b6d953ec), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
#Missing GitSha1 aliases
  $ aliasverify verify git-sha1 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias GitSha1(7371f47a6f8bd23a8fa1a8b2a9479cdd76380e54), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
  [DEBUG] Missing alias blob: alias GitSha1(8c7e5a667f1b771847fe88c01c3de34413a1b220), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias GitSha1(96d80cd6c4e7158dbebd0849f4fb7ce513e5828c), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))

  $ ls $TESTTMP/blobstore/blobs | grep "alias" | wc -l
  0

#Generate Sha256 aliases
  $ aliasverify generate sha256 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias Sha256(559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias Sha256(6b23c0d5f35d1b11f9b683f0b0a617355deb11277d91ae091d399c655b87940d), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))
  [DEBUG] Missing alias blob: alias Sha256(df7e70e5021544f4834bbee64a9e3789febc4be81470df629cad6ddb03320a5c), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
#Generate SeededBlake3 aliases
  $ aliasverify generate seeded-blake3 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias Blake3(5667f2421ac250c4bb9af657b5ead3cdbd940bfbc350b2bfee47454643832b48), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
  [DEBUG] Missing alias blob: alias Blake3(5ad3ba58a716e5fc04296ac9af7a1420f726b401fdf16d270beb5b6b30bc0cda), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias Blake3(6fb4c384e79ac0771a483fcf3c46fb4ea8609f79608e8bcbf710f9887a3b9cf6), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))
#Generate Sha1 aliases
  $ aliasverify generate sha1 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias Sha1(32096c2e0eff33d844ee6d675407ace18289357d), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))
  [DEBUG] Missing alias blob: alias Sha1(6dcd4ce23d88e2ee9568ba546c007c63d9131c1b), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias Sha1(ae4f281df5a5d0ff3cad6371f76d5c29b6d953ec), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
#Generate GitSha1 aliases
  $ aliasverify generate git-sha1 --debug 2>&1 | grep "Missing alias blob" | sort
  [DEBUG] Missing alias blob: alias GitSha1(7371f47a6f8bd23a8fa1a8b2a9479cdd76380e54), content_id ContentId(Blake2(55662471e2a28db8257939b2f9a2d24e65b46a758bac12914a58f17dcde6905f))
  [DEBUG] Missing alias blob: alias GitSha1(8c7e5a667f1b771847fe88c01c3de34413a1b220), content_id ContentId(Blake2(eb56488e97bb4cf5eb17f05357b80108a4a71f6c3bab52dfcaec07161d105ec9))
  [DEBUG] Missing alias blob: alias GitSha1(96d80cd6c4e7158dbebd0849f4fb7ce513e5828c), content_id ContentId(Blake2(896ad5879a5df0403bfc93fc96507ad9c93b31b11f3d0fa05445da7918241e5d))

  $ ls $TESTTMP/blobstore/blobs | grep "alias"
  blob-repo0000.alias.gitsha1.7371f47a6f8bd23a8fa1a8b2a9479cdd76380e54
  blob-repo0000.alias.gitsha1.8c7e5a667f1b771847fe88c01c3de34413a1b220
  blob-repo0000.alias.gitsha1.96d80cd6c4e7158dbebd0849f4fb7ce513e5828c
  blob-repo0000.alias.seeded_blake3.5667f2421ac250c4bb9af657b5ead3cdbd940bfbc350b2bfee47454643832b48
  blob-repo0000.alias.seeded_blake3.5ad3ba58a716e5fc04296ac9af7a1420f726b401fdf16d270beb5b6b30bc0cda
  blob-repo0000.alias.seeded_blake3.6fb4c384e79ac0771a483fcf3c46fb4ea8609f79608e8bcbf710f9887a3b9cf6
  blob-repo0000.alias.sha1.32096c2e0eff33d844ee6d675407ace18289357d
  blob-repo0000.alias.sha1.6dcd4ce23d88e2ee9568ba546c007c63d9131c1b
  blob-repo0000.alias.sha1.ae4f281df5a5d0ff3cad6371f76d5c29b6d953ec
  blob-repo0000.alias.sha256.559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd
  blob-repo0000.alias.sha256.6b23c0d5f35d1b11f9b683f0b0a617355deb11277d91ae091d399c655b87940d
  blob-repo0000.alias.sha256.df7e70e5021544f4834bbee64a9e3789febc4be81470df629cad6ddb03320a5c

Aliasverify declares no service, so with a deployment it loads COMMAND's view,
even of a deployment without an aliasverify service:
  $ DEPLOYMENTS="$LOCAL_CONFIGERATOR_PATH/scm/mononoke/deployments"
  $ jq 'del(.services["23"])' "$DEPLOYMENTS/$MONONOKE_CONFIG_TIER" > "$DEPLOYMENTS/no_aliasverify"
  $ GLOG_minloglevel=5 "$MONONOKE_ALIAS_VERIFY" --repo-id $REPOID \
  >   "${CACHE_ARGS[@]}" "${COMMON_ARGS[@]}" --deployment no_aliasverify \
  >   --alias-type sha256 --mode verify --tracing-test-format 2>&1 | grep "Alias Verification"
  [INFO] Alias Verification continues: 0 errors found
  [INFO] Alias Verification finished: 0 errors found
