/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use diff as diff_feature;
use diff_service_if as thrift;
use ephemeral_blobstore::BubbleId;
use mercurial_types::HgChangesetId;
use mononoke_api::ChangesetContext;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::RepoContext;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::Globalrev;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::P4ChangelistId;
use mononoke_types::Svnrev;
use mononoke_types::hash::GitSha1;

use crate::errors::ServerError;
use crate::repo::Repo;

pub(crate) fn diff_error(reason: impl Into<String>) -> ServerError {
    ServerError::ThriftError(thrift::RequestErrorReason::diff_error(thrift::DiffError {
        reason: reason.into(),
    }))
}

/// Convert thrift CommitId to ChangesetSpecifier for resolution.
pub fn commit_id_to_specifier(
    commit_id: &thrift::CommitId,
) -> Result<ChangesetSpecifier, ServerError> {
    match commit_id {
        thrift::CommitId::bonsai(bytes) => {
            let cs_id = ChangesetId::from_bytes(bytes)
                .map_err(|e| diff_error(format!("Invalid bonsai changeset id: {e}")))?;
            Ok(ChangesetSpecifier::Bonsai(cs_id))
        }
        thrift::CommitId::hg(bytes) => {
            let hg_cs_id = HgChangesetId::from_bytes(bytes)
                .map_err(|e| diff_error(format!("Invalid hg changeset id: {e}")))?;
            Ok(ChangesetSpecifier::Hg(hg_cs_id))
        }
        thrift::CommitId::git(bytes) => {
            let git_sha1 = GitSha1::from_bytes(bytes)
                .map_err(|e| diff_error(format!("Invalid git sha1: {e}")))?;
            Ok(ChangesetSpecifier::GitSha1(git_sha1))
        }
        thrift::CommitId::globalrev(rev) => {
            let rev: u64 = (*rev)
                .try_into()
                .map_err(|_| diff_error(format!("Invalid globalrev: {rev}")))?;
            Ok(ChangesetSpecifier::Globalrev(Globalrev::new(rev)))
        }
        thrift::CommitId::svnrev(rev) => {
            let rev: u64 = (*rev)
                .try_into()
                .map_err(|_| diff_error(format!("Invalid svn revision: {rev}")))?;
            Ok(ChangesetSpecifier::Svnrev(Svnrev::new(rev)))
        }
        thrift::CommitId::p4_changelist(cl) => {
            let cl: u64 = (*cl)
                .try_into()
                .map_err(|_| diff_error(format!("Invalid p4 changelist: {cl}")))?;
            Ok(ChangesetSpecifier::P4Changelist(P4ChangelistId::new(cl)))
        }
        thrift::CommitId::ephemeral_bonsai(ephemeral) => {
            let cs_id = ChangesetId::from_bytes(&ephemeral.bonsai_id)
                .map_err(|e| diff_error(format!("Invalid ephemeral bonsai changeset id: {e}")))?;
            let bubble_id = if ephemeral.bubble_id == 0 {
                None
            } else {
                Some(BubbleId::try_from(ephemeral.bubble_id).map_err(|_| {
                    diff_error(format!("Invalid bubble_id: {}", ephemeral.bubble_id))
                })?)
            };
            Ok(ChangesetSpecifier::EphemeralBonsai(cs_id, bubble_id))
        }
        thrift::CommitId::UnknownField(_) => Err(diff_error("Unknown commit_id variant")),
    }
}

/// Resolve a CommitId to a ChangesetContext using the repository.
pub async fn resolve_changeset(
    repo: &RepoContext<Repo>,
    commit_id: &thrift::CommitId,
) -> Result<ChangesetContext<Repo>, ServerError> {
    let specifier = commit_id_to_specifier(commit_id)?;
    repo.changeset(specifier)
        .await?
        .ok_or_else(|| diff_error(format!("commit not found: {commit_id:?}")))
}

/// Convert raw binary ContentId to mononoke ContentId.
pub fn convert_content_id(content_id: &[u8]) -> Result<ContentId, ServerError> {
    ContentId::from_bytes(content_id).map_err(|e| diff_error(format!("Invalid content id: {e}")))
}

fn max_string_input_size_mb() -> usize {
    justknobs::get_as::<usize>("scm/mononoke:max_diff_string_size_mb", None)
}

/// Convert thrift DiffSingleInput to diff feature DiffSingleInput.
/// Requires async because CommitId resolution may need repo access.
///
/// Returns `Ok(None)` when a `commit_path` input is denied by
/// restricted-path enforcement and partial responses are allowed: the
/// caller maps that side to absent and sets the partial-response signal.
/// When partial responses are not allowed, the denial fails like any other
/// error. Repo-auth denials and all other errors still fail. `str` and
/// `content_id` inputs carry no path to check and always return `Some`.
pub async fn convert_diff_single_input(
    repo: &RepoContext<Repo>,
    input: thrift::DiffSingleInput,
    policy: &RestrictedPathsPolicy,
) -> Result<Option<diff_feature::types::DiffSingleInput>, ServerError> {
    match input {
        thrift::DiffSingleInput::str(string_input) => {
            let len = string_input.content.len() / (1024 * 1024);
            let max = max_string_input_size_mb();
            if len >= max {
                Err(diff_error(format!(
                    "String input is {len}MB long, goes over the limit of {max} MB"
                )))
            } else {
                Ok(Some(diff_feature::types::DiffSingleInput::String(
                    diff_feature::types::DiffInputString {
                        content: string_input.content,
                    },
                )))
            }
        }
        thrift::DiffSingleInput::commit_path(commit_input) => {
            let changeset = resolve_changeset(repo, &commit_input.commit_id).await?;
            let non_root_path =
                if justknobs::eval("scm/mononoke:enable_diff_service_path_acls", None, None) {
                    match changeset
                        .path_with_content(MPath::new(&commit_input.path)?)
                        .await
                    {
                        Ok(changeset_path) => {
                            NonRootMPath::try_from(changeset_path.path().clone())?
                        }
                        Err(e) if e.is_restricted_path_denial() => {
                            // Strict: the denial fails the whole request.
                            match policy {
                                RestrictedPathsPolicy::Strict => return Err(e.into()),
                                RestrictedPathsPolicy::SkipAndCollect(collector) => {
                                    if let Some(denial) = e.restricted_paths_denial() {
                                        collector.record(denial.denying_restriction());
                                    }
                                    return Ok(None);
                                }
                            }
                        }
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    NonRootMPath::try_from(MPath::new(&commit_input.path)?)?
                };
            let replacement_path = if let Some(replacement) = &commit_input.replacement_path {
                let replacement_mpath = MPath::new(replacement)?;
                Some(NonRootMPath::try_from(replacement_mpath)?)
            } else {
                None
            };

            Ok(Some(diff_feature::types::DiffSingleInput::ChangesetPath(
                diff_feature::types::DiffInputChangesetPath {
                    changeset_id: changeset.id(),
                    path: non_root_path,
                    replacement_path,
                },
            )))
        }
        thrift::DiffSingleInput::content_id(content_input) => {
            let content_id = convert_content_id(&content_input.content_id)?;
            let path = if let Some(path_str) = &content_input.path {
                let path = MPath::new(path_str)?;
                Some(NonRootMPath::try_from(path)?)
            } else {
                None
            };
            let lfs_pointer = content_input
                .lfs_pointer
                .as_ref()
                .map(convert_lfs_pointer_from_thrift)
                .transpose()?;

            Ok(Some(diff_feature::types::DiffSingleInput::Content(
                diff_feature::types::DiffInputContent {
                    content_id,
                    path,
                    lfs_pointer,
                },
            )))
        }
        thrift::DiffSingleInput::UnknownField(_) => Err(diff_error("Unknown input type")),
    }
}

fn convert_lfs_pointer_from_thrift(
    lfs_pointer: &thrift::LfsPointer,
) -> Result<diff_feature::types::LfsPointer, ServerError> {
    Ok(diff_feature::types::LfsPointer {
        sha256: lfs_pointer.sha256.clone(),
        size: lfs_pointer.size,
    })
}

pub fn convert_copy_info(copy_info: thrift::DiffCopyInfo) -> diff_feature::types::DiffCopyInfo {
    match copy_info {
        thrift::DiffCopyInfo::NONE => diff_feature::types::DiffCopyInfo::None,
        thrift::DiffCopyInfo::MOVE => diff_feature::types::DiffCopyInfo::Move,
        thrift::DiffCopyInfo::COPY => diff_feature::types::DiffCopyInfo::Copy,
        _ => diff_feature::types::DiffCopyInfo::None, // Default fallback for unknown values
    }
}

pub fn convert_file_type(file_type: thrift::DiffFileType) -> diff_feature::types::DiffFileType {
    match file_type {
        thrift::DiffFileType::REGULAR => diff_feature::types::DiffFileType::Regular,
        thrift::DiffFileType::EXECUTABLE => diff_feature::types::DiffFileType::Executable,
        thrift::DiffFileType::SYMLINK => diff_feature::types::DiffFileType::Symlink,
        thrift::DiffFileType::GIT_SUBMODULE => diff_feature::types::DiffFileType::GitSubmodule,
        _ => diff_feature::types::DiffFileType::Regular, // Default fallback for unknown values
    }
}

/// Convert diff feature MetadataDiff to thrift MetadataDiffResponse
pub fn convert_metadata_diff_to_thrift(
    diff: diff_feature::types::MetadataDiff,
) -> thrift::MetadataDiffResponse {
    thrift::MetadataDiffResponse {
        base_file_info: convert_metadata_file_info_to_thrift(diff.base_file_info),
        other_file_info: convert_metadata_file_info_to_thrift(diff.other_file_info),
        lines_count: diff.lines_count.map(convert_metadata_lines_count_to_thrift),
        ..Default::default()
    }
}

fn convert_metadata_file_info_to_thrift(
    info: diff_feature::types::MetadataFileInfo,
) -> thrift::DiffFileInfo {
    thrift::DiffFileInfo {
        file_type: info.file_type.map(convert_diff_file_type_to_thrift),
        content_type: info.content_type.map(convert_diff_content_type_to_thrift),
        generated_status: info
            .generated_status
            .map(convert_diff_generated_status_to_thrift),
        ..Default::default()
    }
}

fn convert_metadata_lines_count_to_thrift(
    lines_count: diff_feature::types::MetadataLinesCount,
) -> thrift::DiffLinesCount {
    thrift::DiffLinesCount {
        added_lines: lines_count.added_lines,
        deleted_lines: lines_count.deleted_lines,
        significant_added_lines: lines_count.significant_added_lines,
        significant_deleted_lines: lines_count.significant_deleted_lines,
        first_added_line_number: lines_count.first_added_line_number,
        ..Default::default()
    }
}

fn convert_diff_file_type_to_thrift(
    file_type: diff_feature::types::DiffFileType,
) -> thrift::DiffFileType {
    match file_type {
        diff_feature::types::DiffFileType::Regular => thrift::DiffFileType::REGULAR,
        diff_feature::types::DiffFileType::Executable => thrift::DiffFileType::EXECUTABLE,
        diff_feature::types::DiffFileType::Symlink => thrift::DiffFileType::SYMLINK,
        diff_feature::types::DiffFileType::GitSubmodule => thrift::DiffFileType::GIT_SUBMODULE,
    }
}

fn convert_diff_content_type_to_thrift(
    content_type: diff_feature::types::DiffContentType,
) -> thrift::DiffContentType {
    match content_type {
        diff_feature::types::DiffContentType::Text => thrift::DiffContentType::TEXT,
        diff_feature::types::DiffContentType::Binary => thrift::DiffContentType::BINARY,
        diff_feature::types::DiffContentType::NonUtf8 => thrift::DiffContentType::NON_UTF8,
        diff_feature::types::DiffContentType::LfsPointer => thrift::DiffContentType::LFS_POINTER,
    }
}

fn convert_diff_generated_status_to_thrift(
    generated_status: diff_feature::types::DiffGeneratedStatus,
) -> thrift::DiffGeneratedStatus {
    match generated_status {
        diff_feature::types::DiffGeneratedStatus::NonGenerated => {
            thrift::DiffGeneratedStatus::NON_GENERATED
        }
        diff_feature::types::DiffGeneratedStatus::Partially => {
            thrift::DiffGeneratedStatus::PARTIALLY
        }
        diff_feature::types::DiffGeneratedStatus::Fully => thrift::DiffGeneratedStatus::FULLY,
    }
}

pub fn input_changeset_specifier(
    input: Option<&thrift::DiffSingleInput>,
) -> Result<Option<ChangesetSpecifier>, ServerError> {
    match input {
        Some(thrift::DiffSingleInput::commit_path(cp)) => {
            commit_id_to_specifier(&cp.commit_id).map(Some)
        }
        _ => Ok(None),
    }
}

/// Extract a canonical changeset ID for logging without resolving other ID schemes.
pub fn try_extract_changeset_id_sync(input: &thrift::DiffSingleInput) -> Option<ChangesetId> {
    match input {
        thrift::DiffSingleInput::commit_path(cp) => match &cp.commit_id {
            thrift::CommitId::bonsai(bytes) => ChangesetId::from_bytes(bytes).ok(),
            thrift::CommitId::ephemeral_bonsai(ephemeral) => {
                ChangesetId::from_bytes(&ephemeral.bonsai_id).ok()
            }
            _ => None,
        },
        _ => None,
    }
}
