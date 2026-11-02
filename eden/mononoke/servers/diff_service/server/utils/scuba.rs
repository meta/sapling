/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use diff_service_if as thrift;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::utils::diff_conversion::convert_content_id;
use crate::utils::diff_conversion::try_extract_changeset_id_sync;

/// Add estimated input size and inflight_bytes to scuba.
pub fn log_estimated_input_size(
    scuba: &mut MononokeScubaSampleBuilder,
    estimated_size: u64,
    inflight_bytes: &AtomicU64,
) {
    scuba.add("inflight_bytes", inflight_bytes.load(Ordering::Relaxed));
    scuba.add("estimated_input_size", estimated_size);
}

/// Log scuba parameters from a single DiffSingleInput.
///
/// `path_key`, `string_key`, and `lfs_key` control the scuba column names,
/// allowing callers to distinguish base vs other inputs (e.g. "path" vs "other_path").
pub fn log_single_input_scuba(
    scuba: &mut MononokeScubaSampleBuilder,
    input: &thrift::DiffSingleInput,
    path_key: &str,
    string_key: &str,
    lfs_key: &str,
) {
    match input {
        thrift::DiffSingleInput::commit_path(cp) => {
            scuba.add(path_key, cp.path.clone());
        }
        thrift::DiffSingleInput::content_id(ch) => {
            if let Some(path) = &ch.path {
                scuba.add(path_key, path.clone());
            }
            if ch.lfs_pointer.is_some() {
                scuba.add(lfs_key, true);
            }
        }
        thrift::DiffSingleInput::str(_) => {
            scuba.add(string_key, true);
        }
        thrift::DiffSingleInput::UnknownField(_) => {}
    }
}

/// Log content IDs and changeset IDs from diff inputs to scuba.
pub fn log_input_ids(scuba: &mut MononokeScubaSampleBuilder, input: &thrift::DiffInput) {
    if let Some(thrift::DiffSingleInput::content_id(content)) = &input.base {
        if let Ok(content_id) = convert_content_id(&content.content_id) {
            scuba.add("base_content_id", content_id.to_string());
        }
    }
    if let Some(thrift::DiffSingleInput::content_id(content)) = &input.other {
        if let Ok(content_id) = convert_content_id(&content.content_id) {
            scuba.add("other_content_id", content_id.to_string());
        }
    }
    if let Some(base) = &input.base {
        if let Some(changeset_id) = try_extract_changeset_id_sync(base) {
            scuba.add("commit", changeset_id.to_string());
        }
    }
    if let Some(other) = &input.other {
        if let Some(changeset_id) = try_extract_changeset_id_sync(other) {
            scuba.add("other_commit", changeset_id.to_string());
        }
    }
}
