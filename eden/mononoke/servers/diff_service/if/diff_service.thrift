/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

include "eden/mononoke/scs/if/source_control.thrift"
include "thrift/annotation/rust.thrift"
include "thrift/annotation/thrift.thrift"

@thrift.AllowLegacyMissingUris
package;

namespace cpp2 facebook.scm.diff_service
namespace php DiffService
namespace py scm.diff_service.thrift
namespace py3 scm.diff_service.thrift
namespace java.swift com.facebook.scm.diff_service

// ============================================================================
// ID Types - imported from source_control.thrift
// ============================================================================

typedef source_control.CommitId CommitId
typedef source_control.EphemeralBonsai EphemeralBonsai
typedef source_control.CommitCompareParams CommitCompareParams
typedef source_control.CommitCompareResponse CommitCompareResponse

/// Content identifier - a 32-byte Blake2 hash of file content.
typedef binary ContentId

// ============================================================================
// Service Definition
// ============================================================================

@rust.RequestContext
service DiffService {
  DiffHunksResponse, stream<DiffHunksData> diff_hunks(
    1: DiffHunksRequest request,
  ) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );

  DiffUnifiedResponse, stream<
    DiffUnifiedChunk throws (1: RequestError ex, 2: InternalError internal_error)
  > diff_unified(1: DiffUnifiedRequest request) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );

  DiffUnifiedHeaderlessResponse, stream<
    DiffUnifiedHeaderlessChunk throws (
      1: RequestError ex,
      2: InternalError internal_error,
    )
  > diff_unified_headerless(1: DiffUnifiedHeaderlessRequest request) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );

  DiffUnifiedUnaryResponse diff_unified_unary(
    1: DiffUnifiedRequest request,
  ) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );

  DiffUnifiedHeaderlessUnaryResponse diff_unified_headerless_unary(
    1: DiffUnifiedHeaderlessRequest request,
  ) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );

  MetadataDiffResponse metadata_diff(1: MetadataDiffRequest request) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );

  CommitCompareResponse commit_compare(1: CommitCompareRequest request) throws (
    1: RequestError ex,
    2: InternalError internal_error,
    3: OverloadedError overloaded,
    4: RepoNotLoadedError repo_not_loaded,
  );
}

@rust.Exhaustive
struct DiffError {
  1: string reason;
}

enum TransientErrorType {
  REPO_NOT_FOUND = 1,
  OVERLOADED = 2,
}

@rust.Exhaustive
struct TransientError {
  1: TransientErrorType error_type;
  2: string message;
}

union RequestErrorReason {
  1: DiffError diff_error;
  2: TransientError transient_error;
}

@rust.Exhaustive
safe permanent server exception RequestError {
  1: RequestErrorReason reason;
}

@rust.Exhaustive
safe permanent server exception InternalError {
  1: string reason;
}

// Dedicated top-level exception so ServiceRouter can retry memory admission
// failures without retrying permanent RequestError variants.
@rust.Exhaustive
safe transient server exception OverloadedError {
  @thrift.ExceptionMessage
  1: string message;
}

// The repository belongs to the tier but is not loaded on the selected shard.
// This is distinct from a permanent request for a repository that does not exist.
@rust.Exhaustive
safe transient server exception RepoNotLoadedError {
  @thrift.ExceptionMessage
  1: string message;
}

enum DiffCopyInfo {
  NONE = 1,
  MOVE = 2,
  COPY = 3,
}

enum DiffFileType {
  REGULAR = 1,
  EXECUTABLE = 2,
  SYMLINK = 3,
  GIT_SUBMODULE = 4,
}

enum DiffMode {
  INLINE = 1,
  OMIT_CONTENT = 2,
}

// LFS pointer structure with sha256 and size
struct LfsPointer {
  1: string sha256;
  2: i64 size;
}

// General diff input structures
@thrift.ReserveIds{ids = [1, 2]}
union DiffSingleInput {
  3: DiffInputString str;
  4: DiffInputCommitPath commit_path;
  5: DiffInputContentId content_id;
}

// Input structs using source_control.thrift types
struct DiffInputCommitPath {
  1: CommitId commit_id;
  2: string path;
  3: optional string replacement_path;
}

struct DiffInputContentId {
  1: ContentId content_id;
  2: optional string path;
  3: optional LfsPointer lfs_pointer;
}

struct DiffInputString {
  1: string content;
}

struct DiffInput {
  1: optional DiffSingleInput base;
  2: optional DiffSingleInput other;
}

// Diff hunks structures
struct DiffHunksRequest {
  1: string repo_name;
  2: DiffInput input;
  3: bool ignore_whitespace = false;
}

struct DiffHunksResponse {
  1: i64 num_hunks;
  /// Partial-response verdict. See source_control.PartialResponseInfo.
  2: optional source_control.PartialResponseInfo partial_info;
}

struct HunkRange {
  1: i64 start;
  2: i64 end;
}

struct DiffHunksData {
  1: HunkRange add_range;
  2: HunkRange remove_range;
}

// Unified diff structures
struct DiffUnifiedRequest {
  1: string repo_name;
  2: DiffInput input;
  3: DiffUnifiedOptions options;
}

struct DiffUnifiedOptions {
  1: i32 context_lines = 3;
  2: DiffMode diff_mode;
  3: DiffFileType file_type;
  4: DiffCopyInfo copy_info;
  5: bool inspect_lfs_pointers;
  6: bool ignore_whitespace = false;
}

struct DiffUnifiedResponse {
  1: bool is_binary;
  2: i64 num_chunks;
  /// Partial-response verdict. See source_control.PartialResponseInfo.
  3: optional source_control.PartialResponseInfo partial_info;
}

struct DiffUnifiedChunk {
  1: binary content;
}

// Headerless diff structures
struct DiffUnifiedHeaderlessRequest {
  1: string repo_name;
  2: DiffInput input;
  3: optional DiffUnifiedHeaderlessOptions options;
}

struct DiffUnifiedHeaderlessOptions {
  1: i32 context_lines = 3;
  2: bool inspect_binary_data;
  3: bool ignore_whitespace = false;
}

struct DiffUnifiedHeaderlessResponse {
  1: bool is_binary;
  2: i64 num_chunks;
  /// Partial-response verdict. See source_control.PartialResponseInfo.
  3: optional source_control.PartialResponseInfo partial_info;
}

struct DiffUnifiedHeaderlessChunk {
  1: binary content;
}

// Unary unified diff response — single-shot equivalent of `diff_unified` that
// returns the full body in one Thrift message instead of a streamed sequence
// of `DiffUnifiedChunk`s. Saves the per-chunk envelope and the SCS-side
// reassembly buffer for callers that already collect the stream into a Vec.
struct DiffUnifiedUnaryResponse {
  1: bool is_binary;
  2: binary content;
  /// Partial-response verdict. See source_control.PartialResponseInfo.
  3: optional source_control.PartialResponseInfo partial_info;
}

// Unary equivalent of `diff_unified_headerless`. Same rationale as
// DiffUnifiedUnaryResponse.
struct DiffUnifiedHeaderlessUnaryResponse {
  1: bool is_binary;
  2: binary content;
  /// Partial-response verdict. See source_control.PartialResponseInfo.
  3: optional source_control.PartialResponseInfo partial_info;
}

// Metadata Diff structures

enum DiffContentType {
  TEXT = 1,
  NON_UTF8 = 2,
  BINARY = 3,
  LFS_POINTER = 4,
}

enum DiffGeneratedStatus {
  FULLY = 1,
  PARTIALLY = 2,
  NON_GENERATED = 3,
}

struct DiffFileInfo {
  1: optional DiffFileType file_type;
  2: optional DiffContentType content_type;
  3: optional DiffGeneratedStatus generated_status;
}

struct DiffLinesCount {
  1: i64 added_lines;
  2: i64 deleted_lines;
  3: i64 significant_added_lines;
  4: i64 significant_deleted_lines;
  5: optional i64 first_added_line_number;
}

struct MetadataDiffResponse {
  1: DiffFileInfo base_file_info;
  2: DiffFileInfo other_file_info;
  3: optional DiffLinesCount lines_count;
  /// Partial-response verdict. See source_control.PartialResponseInfo.
  4: optional source_control.PartialResponseInfo partial_info;
}

struct MetadataDiffRequest {
  1: string repo_name;
  2: DiffInput input;
  3: bool ignore_whitespace = false;
}

// Commit compare structures

struct CommitCompareRequest {
  1: string repo_name;
  2: CommitId commit_id;
  3: CommitCompareParams params;
}
