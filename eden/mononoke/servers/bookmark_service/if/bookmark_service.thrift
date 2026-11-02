/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

include "eden/mononoke/mononoke_types/serialization/id.thrift"
include "thrift/annotation/rust.thrift"
include "thrift/annotation/thrift.thrift"

@thrift.AllowLegacyMissingUris
package;

namespace py3 eden.mononoke.facebook.bookmark_service

/// In sync with BookmarkCacheDerivedData in cmdlib/environment crate.
enum DerivedDataSet {
  // Only the derived data relevant to mononoke/server (historically only hg)
  HG_ONLY = 0,
  // Only the derived data relevant to Git
  GIT_ONLY = 1,
  // All configured derived data types
  ALL_KINDS = 2,
}

@rust.Exhaustive
struct GetRequest {
  1: string repo_name;
  2: string bookmark_name;
  3: optional DerivedDataSet derived_data_types;
}

@rust.Exhaustive
struct GetResponse {
  1: optional id.ChangesetId changeset_id;
}

@rust.Exhaustive
struct ListBookmarksRequest {
  1: string repo_name;
  // Name filter fox prefixes. Empty means no filter.
  2: string prefix;
  /// If set filters bookmarks to names lexicographically greater than given one (exclusive)
  3: optional string after;
  4: optional i64 limit;
  5: optional DerivedDataSet derived_data_types;
}

enum BookmarkKind {
  SCRATCH = 0,
  PUBLISHING = 1,
  PULL_DEFAULT_PUBLISHING = 2,
}

@rust.Exhaustive
struct BookmarkSpec {
  1: string name;
  2: id.ChangesetId cs_id;
  3: BookmarkKind kind;
}

@rust.Exhaustive
struct ListBookmarksResponse {
  1: list<BookmarkSpec> bookmarks;
}

@rust.Exhaustive
struct RepoNotFound {
  1: string reason;
}

union RequestErrorReason {
  1: RepoNotFound repo_not_found;
}

@rust.Exhaustive
safe permanent client exception RequestError {
  1: RequestErrorReason reason;
}

@rust.Exhaustive
safe permanent server exception InternalError {
  1: string reason;
}

@rust.RequestContext
service BookmarkService {
  /// Get cached bookmark value
  GetResponse get(1: GetRequest request) throws (
    1: RequestError request_error,
    2: InternalError internal_error,
  );
  /// List bookmarks
  // (couldn't be named "list" because that's a keyword in thrift)
  ListBookmarksResponse list_bookmarks(1: ListBookmarksRequest request) throws (
    1: RequestError request_error,
    2: InternalError internal_error,
  );
}
