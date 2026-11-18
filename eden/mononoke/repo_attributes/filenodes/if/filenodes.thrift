/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

include "eden/mononoke/mononoke_types/serialization/path.thrift"
include "eden/mononoke/mercurial/types/if/mercurial_thrift.thrift"
include "thrift/annotation/rust.thrift"
include "thrift/annotation/thrift.thrift"

@thrift.AllowLegacyMissingUris
package;

/// Code version used in memcache keys.  This should be changed whenever
/// the layout of memcache entries is changed in an incompatible way.
/// The corresponding sitever, which can be used to flush memcache, is
/// in the JustKnob scm/mononoke_memcache_sitevers:filenodes.
const i32 MC_CODEVER = 3;

union FilenodeInfoList {
  1: list<FilenodeInfo> Data;
  2: list<i64> Pointers;
  // This actual value is ignored
  3: byte TooBig;
}

@rust.Exhaustive
struct FilenodeInfo {
  // 1: deleted
  2: mercurial_thrift.HgNodeHash filenode;
  3: optional mercurial_thrift.HgNodeHash p1;
  4: optional mercurial_thrift.HgNodeHash p2;
  5: optional FilenodeCopyFrom copyfrom;
  6: mercurial_thrift.HgNodeHash linknode;
}

@rust.Exhaustive
struct FilenodeCopyFrom {
  1: path.RepoPath path;
  2: mercurial_thrift.HgNodeHash filenode;
}

@rust.Exhaustive
struct PreparedFilenode {
  1: path.RepoPath path;
  2: FilenodeInfo info;
}

@rust.Exhaustive
struct FilenodesStageOutput {
  // An absent list means filenodes were disabled.
  1: optional list<PreparedFilenode> records;
}
