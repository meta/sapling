# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Set up config
  $ setup_common_config
  $ setup_configerator_configs
  $ cd $TESTTMP

Initialize test repo
  $ testtool_drawdag -R repo --derive-all --print-hg-hashes << EOF
  > A-B-C-D-E-F
  > # modify: A "foo/x" "line A1\nline A2\n"
  > # delete: B "foo/x"
  > # modify: B "foo/y" "line A1\nline A2\nline B3\n"
  > # modify: C "foo/y" "line C1\nline A2\nline B3\n"
  > # modify: D "foo/bar/y" "line C1\nline A2\nline B3\nline D4\n"
  > # modify: F "foo/bar/y" "line F0\nline F1\nline A2\nline B3\nline D4\n"
  > EOF
  A=1e36b71fc0191fb96f37117250f0492eadd5542f
  B=e5aceac2e8e8ac7b4d04831e6db30fd9d44293fd
  C=71395e2974689cb6838869df65e5b92b5f0ae0c6
  D=b27308a9fec83e72857b55a1029b2f61e964718d
  E=0291ff6c71bf0e625a6fa1314bd5940ab5d21535
  F=efdf4a8cf83f873c4d9f227c8de01529e73a450a

Verify derived data existence
  $ mononoke_admin derived-data -R repo fetch -T inferred_copy_from -i $D
  Derived: c0e990b4099714ec5db24a73fda645a4926e640e3f43e00d4fe0300903970013 -> RootInferredCopyFromId(InferredCopyFromId(Blake2(2c21a6b73d580ae70dc259cbd6fe023f91ef375a9cd75e35f65184e0f8a0862c)))
  $ mononoke_admin blobstore -R repo fetch icf.blake2.2c21a6b73d580ae70dc259cbd6fe023f91ef375a9cd75e35f65184e0f8a0862c
  Key: icf.blake2.2c21a6b73d580ae70dc259cbd6fe023f91ef375a9cd75e35f65184e0f8a0862c
  Ctime: * (glob)
  Size: 63
  
  InferredCopyFrom {
      subentries: ShardedMapV2Node {
          prefix: b"foo\x00bar\x00y",
          value: Some(
              InferredCopyFromEntry {
                  from_csid: ChangesetId(
                      Blake2(6940f6f018c7c21cd92e04ab2c68664e940567fbcb67d6c9cdba8240b91c55bd),
                  ),
                  from_path: MPath("foo/y"),
              },
          ),
          children: {},
      },
  }




  $ mononoke_admin derived-data -R repo fetch -T inferred_copy_from -i $F
  Derived: 250dbd169fcfa356bc459d7d5820cc965e3cad2b9747b551eec3a9036494a2df -> RootInferredCopyFromId(InferredCopyFromId(Blake2(01eb594ca052edc3f4b2346870be58acec07c276b334cc233fe065755305ada5)))
  $ mononoke_admin blobstore -R repo fetch icf.blake2.01eb594ca052edc3f4b2346870be58acec07c276b334cc233fe065755305ada5
  Key: icf.blake2.01eb594ca052edc3f4b2346870be58acec07c276b334cc233fe065755305ada5
  Ctime: * (glob)
  Size: 7
  
  InferredCopyFrom {
      subentries: ShardedMapV2Node {
          prefix: b"",
          value: None,
          children: {},
      },
  }



Start SCS server
  $ start_and_wait_for_scs_server

Query blame
  $ scsc blame --repo repo -i $A --path foo/x
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A1
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A2
  $ scsc blame --repo repo -i $B --path foo/y
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A1
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A2
  e5aceac2e8e8ac7b4d04831e6db30fd9d44293fd: line B3
  $ scsc blame --repo repo -i $C --path foo/y
  71395e2974689cb6838869df65e5b92b5f0ae0c6: line C1
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A2
  e5aceac2e8e8ac7b4d04831e6db30fd9d44293fd: line B3
  $ scsc blame --repo repo -i $D --path foo/bar/y
  71395e2974689cb6838869df65e5b92b5f0ae0c6: line C1
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A2
  e5aceac2e8e8ac7b4d04831e6db30fd9d44293fd: line B3
  b27308a9fec83e72857b55a1029b2f61e964718d: line D4
  $ scsc blame --repo repo -i $E --path foo/bar/y
  71395e2974689cb6838869df65e5b92b5f0ae0c6: line C1
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A2
  e5aceac2e8e8ac7b4d04831e6db30fd9d44293fd: line B3
  b27308a9fec83e72857b55a1029b2f61e964718d: line D4
  $ scsc blame --repo repo -i $F --path foo/bar/y
  efdf4a8cf83f873c4d9f227c8de01529e73a450a: line F0
  efdf4a8cf83f873c4d9f227c8de01529e73a450a: line F1
  1e36b71fc0191fb96f37117250f0492eadd5542f: line A2
  e5aceac2e8e8ac7b4d04831e6db30fd9d44293fd: line B3
  b27308a9fec83e72857b55a1029b2f61e964718d: line D4

Error on non-existent path
  $ scsc blame --repo repo -i $F --path foo/x
  * error: SourceControlService::commit_path_blame failed with request_error(RequestError): RequestError: No such path: foo/x (glob)
  [1]
