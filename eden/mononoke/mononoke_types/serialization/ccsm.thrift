/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! ------------
//! IMPORTANT!!!
//! ------------
//! Do not change the order of the fields! Changing the order of the fields
//! results in compatible but *not* identical serializations, so hashes will
//! change.
//! ------------
//! IMPORTANT!!!
//! ------------

include "eden/mononoke/mononoke_types/serialization/sharded_map.thrift"
include "thrift/annotation/rust.thrift"
include "thrift/annotation/thrift.thrift"

@thrift.AllowLegacyMissingUris
package;

namespace py3 eden.mononoke.mononoke_types.serialization

// Case conflict skeleton manifest stores a version of the file tree that's transformed in a way
// that allows quickly finding case conflicts. The transformation works on file paths by adding
// before each path element its lowercase form:
// <elem1>/<elem2>/.../<elemN> -> lowercase(<elem1>)/<elem1>/lowercase(<elem2>)/<elem2>/.../lowercase(<elemN>)/<elemN>
// In the transformed tree any two conflicting file paths will share a common lowercase form directory,
// and diverge on the next path element.
//
// Example:
// - Dir1/eden/MoNoNOKe -> dir1/Dir1/eden/eden/mononoke/MoNoNOKe
// - Dir1/Eden/SCM -> dir1/Dir1/eden/Eden/scm/SCM
// - Dir1/eden/mononOKE -> dir1/Dir1/eden/eden/mononoke/mononOKE
//
// dir1 -- Dir1 -- eden --- eden -- mononoke -- MoNoNOKe
//                       |
//                       -- Eden --- mononoke -- mononOKE
//                                |
//                                -- scm -- SCM
//
// In this example Dir1/eden directory conflicts Dir1/Eden because they share dir1/Dir1/eden in
// the transform tree and then diverge. Similarly Dir1/eden/MoNoNOKe and Dir1/eden/mononOKE
// conflict because they share dir1/Dir1/eden/eden/mononoke and then diverge.
@rust.Exhaustive
struct CcsmFile {}
@rust.Exhaustive
struct CaseConflictSkeletonManifest {
  1: sharded_map.ShardedMapV2Node subentries;
}

union CcsmEntry {
  1: CcsmFile file;
  2: CaseConflictSkeletonManifest directory;
}

@rust.Exhaustive
struct CcsmRollupCounts {
  1: i64 descendants_count;
  2: i64 odd_depth_conflicts;
  3: i64 even_depth_conflicts;
}

@rust.Exhaustive
struct CcsmStageOutput {
  1: optional CcsmEntry entry;
}
