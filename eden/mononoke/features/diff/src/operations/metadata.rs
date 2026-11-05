/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::ops::Range;
use std::sync::LazyLock;

use anyhow::Context;
use bytes::Bytes;
use context::CoreContext;
use futures::try_join;
use mononoke_macros::mononoke;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
#[cfg(test)]
use mononoke_types::MPath;
#[cfg(test)]
use mononoke_types::NonRootMPath;
use regex::Regex;

use crate::error::DiffError;
use crate::types::DiffContentType;
use crate::types::DiffFileType;
use crate::types::DiffGeneratedStatus;
use crate::types::DiffSingleInput;
use crate::types::MetadataDiff;
use crate::types::MetadataFileInfo;
use crate::types::MetadataLinesCount;
use crate::types::Repo;
use crate::utils::content::get_file_info_from_changeset_path;
use crate::utils::content::get_lfs_pointer;

// This logic comes from `mononoke_api/src/changeset_path_diff.rs`

static BEGIN_MANUAL_SECTION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s|[[:punct:]])*BEGIN MANUAL SECTION").unwrap());
static END_MANUAL_SECTION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s|[[:punct:]])*END MANUAL SECTION").unwrap());

#[derive(Clone, Debug)]
enum FileGeneratedSpan {
    FullyGenerated,
    PartiallyGenerated(Vec<Range<usize>>),
    NotGenerated,
}

#[derive(Clone, Debug)]
struct TextFile {
    file_content: Bytes,
    metadata: ContentMetadataV2,
    generated_span: FileGeneratedSpan,
}

#[derive(Clone, Debug)]
enum ParsedFileContent {
    Text(TextFile),
    NonUtf8,
    Binary,
    LfsPointer,
}

enum LoadedFileContent {
    Text(Bytes, ContentMetadataV2),
    String(Bytes),
    NonUtf8,
    Binary,
    LfsPointer,
}

impl FileGeneratedSpan {
    fn new(content: Bytes, metadata: &ContentMetadataV2) -> Result<Self, DiffError> {
        if !metadata.is_generated && !metadata.is_partially_generated {
            return Ok(FileGeneratedSpan::NotGenerated);
        }

        let content = std::str::from_utf8(&content)
            .context("Failed to parse valid UTF8 bytes for determining generated status")
            .map_err(DiffError::internal)?;

        let mut found_generated_annotation = false;
        let mut manual_sections_ranges = Vec::new();
        let mut manual_section_start = None;

        for (line_number, line) in content.lines().enumerate() {
            if has_generated_annotation(line) {
                found_generated_annotation = true;
            }

            if END_MANUAL_SECTION_REGEX.is_match(line) {
                if let Some(manual_section_start) = manual_section_start {
                    manual_sections_ranges.push(manual_section_start..line_number);
                }
                manual_section_start = None;
            }

            if BEGIN_MANUAL_SECTION_REGEX.is_match(line) {
                manual_section_start = Some(line_number + 1);
            }
        }

        Ok(
            match (
                found_generated_annotation,
                manual_sections_ranges.is_empty(),
            ) {
                (true, true) => FileGeneratedSpan::FullyGenerated,
                (true, false) => FileGeneratedSpan::PartiallyGenerated(manual_sections_ranges),
                (false, _) => FileGeneratedSpan::NotGenerated,
            },
        )
    }
}

/// Markers immediately preceded by a backtick are Markdown code-span references
/// to the marker rather than a declaration that the file is generated.
fn has_generated_annotation(line: &str) -> bool {
    // The redundant concat is used to avoid marking this file as generated.
    [
        concat!("@", "generated"),
        concat!("@", "partially-generated"),
    ]
    .iter()
    .any(|marker| {
        line.match_indices(marker)
            .any(|(idx, _)| !line[..idx].ends_with('`'))
    })
}

impl TextFile {
    fn new(file_content: Bytes, metadata: ContentMetadataV2) -> Result<Self, DiffError> {
        Ok(TextFile {
            generated_span: FileGeneratedSpan::new(file_content.clone(), &metadata)?,
            file_content,
            metadata,
        })
    }

    fn lines(&self) -> usize {
        if self.metadata.ends_in_newline {
            // For files that end in newline, the number of lines is equal to the number of newlines.
            self.metadata.newline_count as usize
        } else if self.metadata.total_size == 0 {
            // For empty files, the number of lines is zero.
            0
        } else {
            // For non-empty files that don't end in a newline, the number of lines is equal to the number of newlines plus one for the last line.
            (self.metadata.newline_count + 1) as usize
        }
    }

    fn significant_lines_count(&self) -> usize {
        match &self.generated_span {
            FileGeneratedSpan::FullyGenerated => 0usize,
            FileGeneratedSpan::PartiallyGenerated(manual_sections) => manual_sections
                .iter()
                .fold(0usize, |acc, section| acc.saturating_add(section.len())),
            FileGeneratedSpan::NotGenerated => self.lines(),
        }
    }

    fn significant_lines_count_in_a_range(&self, range: &Range<usize>) -> usize {
        match &self.generated_span {
            FileGeneratedSpan::FullyGenerated => 0usize,
            FileGeneratedSpan::PartiallyGenerated(manual_sections) => {
                manual_sections.iter().fold(0usize, |acc, section| {
                    acc.saturating_add(
                        section
                            .end
                            .min(range.end)
                            .saturating_sub(section.start.max(range.start)),
                    )
                })
            }
            FileGeneratedSpan::NotGenerated => range.len(),
        }
    }
}

impl LoadedFileContent {
    async fn new(
        ctx: &CoreContext,
        repo: &impl Repo,
        content_id: ContentId,
        is_lfs: bool,
    ) -> Result<Self, DiffError> {
        if is_lfs {
            return Ok(Self::LfsPointer);
        }
        // Load content metadata from blobstore
        let metadata = crate::utils::content::get_content_metadata(ctx, repo, &content_id).await?;

        let content = if metadata.is_binary {
            Self::Binary
        } else if metadata.is_utf8 {
            // Load the actual content
            let file_content = {
                let blobstore = repo.repo_blobstore();
                let fetch_key = filestore::FetchKey::Canonical(content_id);
                filestore::fetch_concat_opt(blobstore, ctx, &fetch_key)
                    .await
                    .map_err(DiffError::internal)?
                    .ok_or_else(|| DiffError::content_not_found(content_id))?
            };
            Self::Text(file_content, metadata)
        } else {
            Self::NonUtf8
        };
        Ok(content)
    }

    fn needs_cpu_work(&self) -> bool {
        matches!(self, Self::Text(..) | Self::String(..))
    }

    fn parse(self) -> Result<ParsedFileContent, DiffError> {
        match self {
            Self::Text(content, metadata) => {
                Ok(ParsedFileContent::Text(TextFile::new(content, metadata)?))
            }
            Self::String(content) => ParsedFileContent::from_string(content),
            Self::Binary => Ok(ParsedFileContent::Binary),
            Self::NonUtf8 => Ok(ParsedFileContent::NonUtf8),
            Self::LfsPointer => Ok(ParsedFileContent::LfsPointer),
        }
    }
}

impl ParsedFileContent {
    fn from_string(file_content: Bytes) -> Result<Self, DiffError> {
        let is_binary = file_content.contains(&0u8);
        let is_utf8 = std::str::from_utf8(&file_content).is_ok();

        // String inputs have no persisted content metadata.
        let newline_count = file_content.iter().filter(|&&b| b == b'\n').count() as u64;
        let ends_in_newline = file_content.last() == Some(&b'\n');
        let metadata = ContentMetadataV2 {
            content_id: ContentId::from_bytes([0; 32]).map_err(DiffError::internal)?,
            total_size: file_content.len() as u64,
            sha1: mononoke_types::hash::Sha1::from_byte_array([0; 20]),
            sha256: mononoke_types::hash::Sha256::from_byte_array([0; 32]),
            git_sha1: mononoke_types::hash::RichGitSha1::from_sha1(
                mononoke_types::hash::GitSha1::from_byte_array([0; 20]),
                "blob",
                file_content.len() as u64,
            ),
            seeded_blake3: mononoke_types::hash::Blake3::from_byte_array([0; 32]),
            is_binary,
            is_utf8,
            is_ascii: file_content.is_ascii(),
            ends_in_newline,
            newline_count,
            first_line: None,
            is_generated: false,
            is_partially_generated: false,
        };

        if is_binary {
            Ok(Self::Binary)
        } else if is_utf8 {
            Ok(Self::Text(TextFile::new(file_content, metadata)?))
        } else {
            Ok(Self::NonUtf8)
        }
    }
}

fn calculate_lines_count(
    old_parsed_file_content: Option<&ParsedFileContent>,
    new_parsed_file_content: Option<&ParsedFileContent>,
    old_file_type: Option<&DiffFileType>,
    new_file_type: Option<&DiffFileType>,
    ignore_whitespace: bool,
) -> Option<MetadataLinesCount> {
    if matches!(old_file_type, Some(&DiffFileType::GitSubmodule))
        || matches!(new_file_type, Some(&DiffFileType::GitSubmodule))
    {
        return None;
    }

    match (old_parsed_file_content, new_parsed_file_content) {
        (
            Some(ParsedFileContent::Text(old_text_file)),
            Some(ParsedFileContent::Text(new_text_file)),
        ) => Some(diff_files(old_text_file, new_text_file, ignore_whitespace)),
        (Some(ParsedFileContent::Text(old_text_file)), _) => Some(file_deleted(old_text_file)),
        (_, Some(ParsedFileContent::Text(new_text_file))) => Some(file_created(new_text_file)),
        _ => None,
    }
}

fn diff_files(
    old_text_file: &TextFile,
    new_text_file: &TextFile,
    ignore_whitespace: bool,
) -> MetadataLinesCount {
    let old_content = if ignore_whitespace {
        Bytes::from(xdiff::strip_horizontal_whitespace(
            &old_text_file.file_content,
        ))
    } else {
        old_text_file.file_content.clone()
    };

    let new_content = if ignore_whitespace {
        Bytes::from(xdiff::strip_horizontal_whitespace(
            &new_text_file.file_content,
        ))
    } else {
        new_text_file.file_content.clone()
    };

    let hunks = xdiff::diff_hunks(old_content, new_content);

    let mut added_lines = 0i64;
    let mut deleted_lines = 0i64;
    let mut significant_added_lines = 0i64;
    let mut significant_deleted_lines = 0i64;
    let mut first_added_line_number = None;

    for hunk in hunks {
        added_lines = added_lines.saturating_add(hunk.add.len() as i64);
        deleted_lines = deleted_lines.saturating_add(hunk.remove.len() as i64);
        significant_added_lines = significant_added_lines
            .saturating_add(new_text_file.significant_lines_count_in_a_range(&hunk.add) as i64);
        significant_deleted_lines = significant_deleted_lines
            .saturating_add(old_text_file.significant_lines_count_in_a_range(&hunk.remove) as i64);
        if !hunk.add.is_empty() && first_added_line_number.is_none() {
            first_added_line_number = Some(hunk.add.start.saturating_add(1) as i64); // +1 because hunk boundaries are 0-based.
        }
    }

    MetadataLinesCount {
        added_lines,
        deleted_lines,
        significant_added_lines,
        significant_deleted_lines,
        first_added_line_number,
    }
}

fn file_created(new_text_file: &TextFile) -> MetadataLinesCount {
    MetadataLinesCount {
        added_lines: new_text_file.lines() as i64,
        deleted_lines: 0,
        significant_added_lines: new_text_file.significant_lines_count() as i64,
        significant_deleted_lines: 0,
        first_added_line_number: if new_text_file.lines() > 0 {
            Some(1)
        } else {
            None
        },
    }
}

fn file_deleted(old_text_file: &TextFile) -> MetadataLinesCount {
    MetadataLinesCount {
        added_lines: 0,
        deleted_lines: old_text_file.lines() as i64,
        significant_added_lines: 0,
        significant_deleted_lines: old_text_file.significant_lines_count() as i64,
        first_added_line_number: None,
    }
}

fn convert_file_type_to_diff(file_type: mononoke_types::FileType) -> DiffFileType {
    match file_type {
        mononoke_types::FileType::Regular => DiffFileType::Regular,
        mononoke_types::FileType::Executable => DiffFileType::Executable,
        mononoke_types::FileType::Symlink => DiffFileType::Symlink,
        mononoke_types::FileType::GitSubmodule => DiffFileType::GitSubmodule,
    }
}

fn convert_content_type_to_diff(parsed_content: &ParsedFileContent) -> DiffContentType {
    match parsed_content {
        ParsedFileContent::Text(_) => DiffContentType::Text,
        ParsedFileContent::NonUtf8 => DiffContentType::NonUtf8,
        ParsedFileContent::Binary => DiffContentType::Binary,
        ParsedFileContent::LfsPointer => DiffContentType::LfsPointer,
    }
}

fn convert_generated_status_to_diff(generated_span: &FileGeneratedSpan) -> DiffGeneratedStatus {
    match generated_span {
        FileGeneratedSpan::FullyGenerated => DiffGeneratedStatus::Fully,
        FileGeneratedSpan::PartiallyGenerated(_) => DiffGeneratedStatus::Partially,
        FileGeneratedSpan::NotGenerated => DiffGeneratedStatus::NonGenerated,
    }
}

fn create_file_info(
    file_type: Option<DiffFileType>,
    parsed_content: Option<&ParsedFileContent>,
) -> MetadataFileInfo {
    let content_type = parsed_content.map(convert_content_type_to_diff);
    let generated_status = match parsed_content {
        Some(ParsedFileContent::Text(text_file)) => {
            Some(convert_generated_status_to_diff(&text_file.generated_span))
        }
        _ => None,
    };

    MetadataFileInfo {
        file_type,
        content_type,
        generated_status,
    }
}

async fn get_file_details_from_input(
    ctx: &CoreContext,
    repo: &impl Repo,
    input: DiffSingleInput,
) -> Result<(Option<DiffFileType>, Option<LoadedFileContent>), DiffError> {
    match input {
        DiffSingleInput::ChangesetPath(changeset_input) => {
            let non_root_mpath = changeset_input.path.clone();

            let file_info = get_file_info_from_changeset_path(
                ctx,
                repo,
                changeset_input.changeset_id,
                non_root_mpath,
            )
            .await?;

            let (content_id, file_type) = match file_info {
                Some((cid, ft)) => (Some(cid), Some(convert_file_type_to_diff(ft))),
                None => (None, None),
            };

            // For Git submodules, we don't parse file content for metadata purposes
            let parsed_file_content = match (&content_id, &file_type) {
                (_, Some(DiffFileType::GitSubmodule)) => None,
                (Some(content_id), _) => {
                    // Check if this is an LFS pointer — if so, report LfsPointer content
                    // type instead of analyzing the actual (potentially binary) content
                    let is_lfs = get_lfs_pointer(
                        ctx,
                        repo,
                        changeset_input.changeset_id,
                        changeset_input.path,
                        content_id,
                    )
                    .await?
                    .is_some();
                    Some(LoadedFileContent::new(ctx, repo, *content_id, is_lfs).await?)
                }
                (None, _) => None,
            };

            Ok((file_type, parsed_file_content))
        }
        DiffSingleInput::Content(content_input) => {
            // If an LFS pointer was provided with this content input, report as LfsPointer
            let parsed_file_content = Some(
                LoadedFileContent::new(
                    ctx,
                    repo,
                    content_input.content_id,
                    content_input.lfs_pointer.is_some(),
                )
                .await?,
            );

            // For content-only inputs, we don't have file type information
            Ok((None, parsed_file_content))
        }
        DiffSingleInput::String(string_input) => Ok((
            Some(DiffFileType::Regular),
            Some(LoadedFileContent::String(Bytes::from(
                string_input.content.into_bytes(),
            ))),
        )),
    }
}

/// Compute metadata diff between two inputs.
///
/// Accepts separate repos for base and other inputs to support cross-bubble diffs.
/// Each repo should be bound to the bubble that contains its changeset (if any).
pub async fn metadata(
    ctx: &CoreContext,
    base_pair: Option<(DiffSingleInput, &impl Repo)>,
    other_pair: Option<(DiffSingleInput, &impl Repo)>,
    ignore_whitespace: bool,
) -> Result<MetadataDiff, DiffError> {
    metadata_with_guard(ctx, base_pair, other_pair, ignore_whitespace, ()).await
}

/// Compute a metadata diff while retaining a caller-owned resource reservation.
///
/// The guard stays with the blocking computation if the awaiting request is
/// cancelled. Inputs and repo bindings have the same meaning as in [`metadata`].
pub async fn metadata_with_guard(
    ctx: &CoreContext,
    base_pair: Option<(DiffSingleInput, &impl Repo)>,
    other_pair: Option<(DiffSingleInput, &impl Repo)>,
    ignore_whitespace: bool,
    guard: impl Send + 'static,
) -> Result<MetadataDiff, DiffError> {
    #[cfg(test)]
    let on_compute = executor_tests::take_on_compute();

    // Get file information directly from inputs
    let (base_file_details, other_file_details) = try_join!(
        async {
            if let Some((base_input, base_repo)) = base_pair {
                get_file_details_from_input(ctx, base_repo, base_input)
                    .await
                    .map(Some)
            } else {
                Ok(Some((None, None)))
            }
        },
        async {
            if let Some((other_input, other_repo)) = other_pair {
                get_file_details_from_input(ctx, other_repo, other_input)
                    .await
                    .map(Some)
            } else {
                Ok(Some((None, None)))
            }
        }
    )?;

    let (base_file_type, base_content) = base_file_details.unwrap_or((None, None));
    let (other_file_type, other_content) = other_file_details.unwrap_or((None, None));
    let needs_cpu_work = base_content
        .iter()
        .chain(other_content.iter())
        .any(LoadedFileContent::needs_cpu_work);

    let compute = move || {
        #[cfg(test)]
        if let Some(on_compute) = on_compute {
            on_compute();
        }

        let base_parsed_content = base_content.map(LoadedFileContent::parse).transpose()?;
        let other_parsed_content = other_content.map(LoadedFileContent::parse).transpose()?;
        let lines_count = calculate_lines_count(
            base_parsed_content.as_ref(),
            other_parsed_content.as_ref(),
            base_file_type.as_ref(),
            other_file_type.as_ref(),
            ignore_whitespace,
        );

        let base_file_info = create_file_info(base_file_type, base_parsed_content.as_ref());
        let other_file_info = create_file_info(other_file_type, other_parsed_content.as_ref());

        Ok(MetadataDiff {
            base_file_info,
            other_file_info,
            lines_count,
        })
    };

    if needs_cpu_work {
        run_metadata_computation(guard, compute).await?
    } else {
        compute()
    }
}

async fn run_metadata_computation<T: Send + 'static>(
    guard: impl Send + 'static,
    compute: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DiffError> {
    mononoke::spawn_blocking(move || {
        // A started blocking task outlives cancellation of its awaiting request.
        let _guard = guard;
        compute()
    })
    .await
    .context("metadata diff blocking task failed")
    .map_err(DiffError::internal)
}

#[cfg(test)]
mod executor_tests;

#[cfg(test)]
mod tests {

    use blobstore::Loadable;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use mononoke_types::FileType;
    use mononoke_types::GitLfs;
    use repo_blobstore::RepoBlobstoreRef;
    use test_repo_factory;
    use tests_utils::BasicTestRepo;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::types::DiffInputChangesetPath;
    use crate::types::DiffInputContent;
    use crate::types::DiffSingleInput;
    use crate::types::LfsPointer as DiffLfsPointer;

    async fn init_test_repo(ctx: &CoreContext) -> Result<BasicTestRepo, DiffError> {
        let repo = test_repo_factory::build_empty(ctx.fb)
            .await
            .map_err(DiffError::internal)?;
        Ok(repo)
    }

    fn create_non_root_path(path: &str) -> Result<NonRootMPath, DiffError> {
        let mpath = MPath::new(path)?;
        let non_root_mpath = NonRootMPath::try_from(mpath)?;
        Ok(non_root_mpath)
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_basic(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Create test commits with different content
        let base_cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", "line1\nline2\nline3\n")
            .commit()
            .await?;

        let other_cs = CreateCommitContext::new(&ctx, &repo, vec![base_cs])
            .add_file("file.txt", "line1\nmodified line2\nline3\n")
            .commit()
            .await?;

        let base_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: base_cs,
            path: create_non_root_path("file.txt")?,
            replacement_path: None,
        });
        let other_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: other_cs,
            path: create_non_root_path("file.txt")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        // Check file info
        assert_eq!(
            metadata_diff.base_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Text)
        );
        assert_eq!(
            metadata_diff.base_file_info.generated_status,
            Some(DiffGeneratedStatus::NonGenerated)
        );

        assert_eq!(
            metadata_diff.other_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );
        assert_eq!(
            metadata_diff.other_file_info.generated_status,
            Some(DiffGeneratedStatus::NonGenerated)
        );

        // Check lines count
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 1);
        assert_eq!(lines_count.deleted_lines, 1);
        assert_eq!(lines_count.significant_added_lines, 1);
        assert_eq!(lines_count.significant_deleted_lines, 1);
        assert_eq!(lines_count.first_added_line_number, Some(2));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_binary_files(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Create test commits with binary content (contains null bytes)
        let base_cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("binary.bin", b"binary\x00content\x01here".as_slice())
            .commit()
            .await?;

        let other_cs = CreateCommitContext::new(&ctx, &repo, vec![base_cs])
            .add_file("binary.bin", b"different\x00binary\x02data".as_slice())
            .commit()
            .await?;

        let base_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: base_cs,
            path: create_non_root_path("binary.bin")?,
            replacement_path: None,
        });
        let other_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: other_cs,
            path: create_non_root_path("binary.bin")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        // Check that content type is binary
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Binary)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Binary)
        );

        // Lines count should be None for binary files
        assert!(metadata_diff.lines_count.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_file_creation(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Test with one empty file and one with content
        let base_cs = CreateCommitContext::new_root(&ctx, &repo).commit().await?;

        let other_cs = CreateCommitContext::new(&ctx, &repo, vec![base_cs])
            .add_file("new_file.txt", "new content\nline2\n")
            .commit()
            .await?;

        let base_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: base_cs,
            path: create_non_root_path("new_file.txt")?,
            replacement_path: None,
        });
        let other_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: other_cs,
            path: create_non_root_path("new_file.txt")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        // Base file doesn't exist
        assert_eq!(metadata_diff.base_file_info.file_type, None);
        assert_eq!(metadata_diff.base_file_info.content_type, None);

        // Other file exists
        assert_eq!(
            metadata_diff.other_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        // Check lines count for file creation
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 2);
        assert_eq!(lines_count.deleted_lines, 0);
        assert_eq!(lines_count.first_added_line_number, Some(1));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_with_none_inputs(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Create a test commit with content
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", "some content\nline2\n")
            .commit()
            .await?;

        let input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: cs,
            path: create_non_root_path("file.txt")?,
            replacement_path: None,
        });

        // Test None vs Some - should show addition
        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((input.clone(), &repo)),
            false,
        )
        .await?;

        // Base file doesn't exist
        assert_eq!(metadata_diff.base_file_info.file_type, None);
        assert_eq!(metadata_diff.base_file_info.content_type, None);

        // Other file exists
        assert_eq!(
            metadata_diff.other_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 2);
        assert_eq!(lines_count.deleted_lines, 0);

        // Test Some vs None - should show deletion
        let metadata_diff = metadata(
            &ctx,
            Some((input, &repo)),
            None::<(DiffSingleInput, &BasicTestRepo)>,
            false,
        )
        .await?;

        // Base file exists
        assert_eq!(
            metadata_diff.base_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Text)
        );

        // Other file doesn't exist
        assert_eq!(metadata_diff.other_file_info.file_type, None);
        assert_eq!(metadata_diff.other_file_info.content_type, None);

        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 0);
        assert_eq!(lines_count.deleted_lines, 2);

        // Test None vs None - should show no difference
        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            false,
        )
        .await?;

        // Both files don't exist
        assert_eq!(metadata_diff.base_file_info.file_type, None);
        assert_eq!(metadata_diff.base_file_info.content_type, None);
        assert_eq!(metadata_diff.other_file_info.file_type, None);
        assert_eq!(metadata_diff.other_file_info.content_type, None);

        // No lines count for non-existent files
        assert!(metadata_diff.lines_count.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_generated_file(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Create a generated file
        let generated_content = concat!(
            "// @",
            "generated\n",
            "Generated content\nMore generated content\n"
        );
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("generated.txt", generated_content)
            .commit()
            .await?;

        let input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: cs,
            path: create_non_root_path("generated.txt")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((input, &repo)),
            false,
        )
        .await?;

        // Check that generated status is detected
        assert_eq!(
            metadata_diff.other_file_info.generated_status,
            Some(DiffGeneratedStatus::Fully)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        // Lines count should consider significant lines (0 for fully generated)
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 3);
        assert_eq!(lines_count.significant_added_lines, 0);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_marker_in_code_span_not_generated(
        fb: FacebookInit,
    ) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        let doc_content = concat!(
            "# Policy\n",
            "**Files with `@",
            "generated SignedSource` header.** Do not edit.\n",
            "Never hand-edit `@",
            "partially-generated` files.\n"
        );
        let header_content = concat!("// @", "generated\n", "See `@", "generated`.\n");
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("doc.md", doc_content)
            .add_file("header.txt", header_content)
            .commit()
            .await?;

        for (path, expected) in [
            ("doc.md", DiffGeneratedStatus::NonGenerated),
            ("header.txt", DiffGeneratedStatus::Fully),
        ] {
            let input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
                changeset_id: cs,
                path: create_non_root_path(path)?,
                replacement_path: None,
            });
            let metadata_diff = metadata(
                &ctx,
                None::<(DiffSingleInput, &BasicTestRepo)>,
                Some((input, &repo)),
                false,
            )
            .await?;
            assert_eq!(
                metadata_diff.other_file_info.generated_status,
                Some(expected),
                "{path}"
            );
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_partially_generated_file(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Create a partially generated file with manual sections
        let partially_generated_content = concat!(
            "// @partially-generated\n",
            "Generated section 1\n",
            "// BEGIN MANUAL SECTION\n",
            "Manual code here\n",
            "More manual code\n",
            "// END MANUAL SECTION\n",
            "Generated section 2\n"
        );
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("partial.txt", partially_generated_content)
            .commit()
            .await?;

        let input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: cs,
            path: create_non_root_path("partial.txt")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((input, &repo)),
            false,
        )
        .await?;

        // Check that partially generated status is detected
        assert_eq!(
            metadata_diff.other_file_info.generated_status,
            Some(DiffGeneratedStatus::Partially)
        );

        // Lines count should only count manual sections as significant
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 7); // Total lines
        assert_eq!(lines_count.significant_added_lines, 2); // Only manual lines (lines 3-4, 0-indexed)

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_string_inputs(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test with String inputs
        let base_input = DiffSingleInput::String(DiffInputString {
            content: "line1\nline2\nline3\n".to_string(),
        });
        let other_input = DiffSingleInput::String(DiffInputString {
            content: "line1\nmodified line2\nline3\n".to_string(),
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        assert_eq!(
            metadata_diff.base_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Text)
        );
        assert_eq!(
            metadata_diff.base_file_info.generated_status,
            Some(DiffGeneratedStatus::NonGenerated)
        );

        assert_eq!(
            metadata_diff.other_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );
        assert_eq!(
            metadata_diff.other_file_info.generated_status,
            Some(DiffGeneratedStatus::NonGenerated)
        );

        // Check lines count
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 1);
        assert_eq!(lines_count.deleted_lines, 1);
        assert_eq!(lines_count.significant_added_lines, 1);
        assert_eq!(lines_count.significant_deleted_lines, 1);
        assert_eq!(lines_count.first_added_line_number, Some(2));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_string_vs_none(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        let string_input = DiffSingleInput::String(DiffInputString {
            content: "some content\nline2\n".to_string(),
        });

        // Test None vs String - should show addition
        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((string_input.clone(), &repo)),
            false,
        )
        .await?;

        // Base file doesn't exist
        assert_eq!(metadata_diff.base_file_info.file_type, None);
        assert_eq!(metadata_diff.base_file_info.content_type, None);

        // Other file exists
        assert_eq!(
            metadata_diff.other_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 2);
        assert_eq!(lines_count.deleted_lines, 0);

        // Test String vs None - should show deletion
        let metadata_diff = metadata(
            &ctx,
            Some((string_input, &repo)),
            None::<(DiffSingleInput, &BasicTestRepo)>,
            false,
        )
        .await?;

        // Base file exists
        assert_eq!(
            metadata_diff.base_file_info.file_type,
            Some(DiffFileType::Regular)
        );
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Text)
        );

        // Other file doesn't exist
        assert_eq!(metadata_diff.other_file_info.file_type, None);
        assert_eq!(metadata_diff.other_file_info.content_type, None);

        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 0);
        assert_eq!(lines_count.deleted_lines, 2);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_string_binary(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test with binary String inputs (contains null bytes)
        let base_input = DiffSingleInput::String(DiffInputString {
            content: String::from_utf8_lossy(b"binary\x00content").to_string(),
        });
        let other_input = DiffSingleInput::String(DiffInputString {
            content: String::from_utf8_lossy(b"different\x00binary").to_string(),
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        // Check that content type is binary
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Binary)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Binary)
        );

        // Lines count should be None for binary files
        assert!(metadata_diff.lines_count.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_string_empty(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test with empty string inputs
        let empty_input = DiffSingleInput::String(DiffInputString {
            content: String::new(),
        });
        let non_empty_input = DiffSingleInput::String(DiffInputString {
            content: "some content\n".to_string(),
        });

        let metadata_diff = metadata(
            &ctx,
            Some((empty_input.clone(), &repo)),
            Some((non_empty_input, &repo)),
            false,
        )
        .await?;

        // Both should be text files
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Text)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        // Check lines count
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 1);
        assert_eq!(lines_count.deleted_lines, 0);

        // Test two empty strings
        let metadata_diff = metadata(
            &ctx,
            Some((empty_input.clone(), &repo)),
            Some((empty_input, &repo)),
            false,
        )
        .await?;
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(lines_count.added_lines, 0);
        assert_eq!(lines_count.deleted_lines, 0);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_string_special_chars(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test with various special characters and Unicode
        let special_input = DiffSingleInput::String(DiffInputString {
            content: "Hello 世界\nTab:\t\nEmoji: 🚀\nQuotes: \"test\" 'test'\n".to_string(),
        });
        let plain_input = DiffSingleInput::String(DiffInputString {
            content: "Plain text\n".to_string(),
        });

        let metadata_diff = metadata(
            &ctx,
            Some((special_input, &repo)),
            Some((plain_input, &repo)),
            false,
        )
        .await?;

        // Should handle special characters as text
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Text)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        // Lines count should work correctly
        let lines_count = metadata_diff.lines_count.unwrap();
        assert!(lines_count.added_lines > 0 || lines_count.deleted_lines > 0);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_ignore_whitespace_only(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test with only whitespace differences
        let base_input = DiffSingleInput::String(DiffInputString {
            content: "hello world\nfoo bar\n".to_string(),
        });
        let other_input = DiffSingleInput::String(DiffInputString {
            content: "hello  world\nfoo\tbar\n".to_string(),
        });

        // With ignore_whitespace: false, should show differences
        let metadata_diff = metadata(
            &ctx,
            Some((base_input.clone(), &repo)),
            Some((other_input.clone(), &repo)),
            false,
        )
        .await?;
        let lines_count = metadata_diff.lines_count.unwrap();
        assert!(
            lines_count.added_lines > 0 || lines_count.deleted_lines > 0,
            "Should show whitespace differences when ignore_whitespace=false"
        );

        // With ignore_whitespace: true, should show no changes in line counts
        // (After stripping whitespace, "helloworld\nfoobar\n" should match on both sides)
        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            true,
        )
        .await?;
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(
            lines_count.added_lines, 0,
            "Should show no added lines when ignore_whitespace=true for whitespace-only changes"
        );
        assert_eq!(
            lines_count.deleted_lines, 0,
            "Should show no deleted lines when ignore_whitespace=true for whitespace-only changes"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_ignore_whitespace_mixed(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test with both whitespace and content differences
        let base_input = DiffSingleInput::String(DiffInputString {
            content: "line1\nline2\nline3\n".to_string(),
        });
        let other_input = DiffSingleInput::String(DiffInputString {
            content: "line1\nmodified  line2\nline3\n".to_string(),
        });

        // With ignore_whitespace: true, should still show content change
        // After stripping whitespace: "line2" vs "modifiedline2" (real difference!)
        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            true,
        )
        .await?;
        let lines_count = metadata_diff.lines_count.unwrap();
        assert_eq!(
            lines_count.added_lines, 1,
            "Should show content changes even with ignore_whitespace=true"
        );
        assert_eq!(
            lines_count.deleted_lines, 1,
            "Should show content changes even with ignore_whitespace=true"
        );
        assert_eq!(lines_count.first_added_line_number, Some(2));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_ignore_whitespace_binary(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        use crate::types::DiffInputString;

        // Test that binary files are not affected by whitespace stripping
        let base_input = DiffSingleInput::String(DiffInputString {
            content: String::from_utf8_lossy(b"binary\x00 content").to_string(),
        });
        let other_input = DiffSingleInput::String(DiffInputString {
            content: String::from_utf8_lossy(b"binary\x00  content").to_string(),
        });

        // Even with ignore_whitespace: true, binary files should not use line counting
        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            true,
        )
        .await?;

        // Binary files should not have line counts
        assert!(
            metadata_diff.lines_count.is_none(),
            "Binary files should not have line counts"
        );
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::Binary)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Binary)
        );

        Ok(())
    }

    async fn init_test_repo_with_lfs(ctx: &CoreContext) -> Result<BasicTestRepo, DiffError> {
        let mut factory =
            test_repo_factory::TestRepoFactory::new(ctx.fb).map_err(DiffError::internal)?;
        factory.with_config_override(|config| {
            config.git_configs.git_lfs_interpret_pointers = true;
        });
        let repo = factory.build().await.map_err(DiffError::internal)?;
        Ok(repo)
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_lfs_changeset_path(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo_with_lfs(&ctx).await?;

        // Create commits with LFS-tracked binary content.
        // With git_lfs_interpret_pointers enabled, the actual content is stored
        // but the file change is marked as an LFS pointer.
        let base_content = b"binary\x00lfs\x01content\x02base".as_slice();
        let other_content = b"binary\x00lfs\x01content\x02other".as_slice();

        let base_cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file_with_type_and_lfs(
                "large_file.bin",
                base_content,
                FileType::Regular,
                GitLfs::canonical_pointer(),
            )
            .commit()
            .await?;

        let other_cs = CreateCommitContext::new(&ctx, &repo, vec![base_cs])
            .add_file_with_type_and_lfs(
                "large_file.bin",
                other_content,
                FileType::Regular,
                GitLfs::canonical_pointer(),
            )
            .commit()
            .await?;

        let base_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: base_cs,
            path: create_non_root_path("large_file.bin")?,
            replacement_path: None,
        });
        let other_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: other_cs,
            path: create_non_root_path("large_file.bin")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        // Both sides should report LfsPointer content type instead of Binary
        assert_eq!(
            metadata_diff.base_file_info.content_type,
            Some(DiffContentType::LfsPointer)
        );
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::LfsPointer)
        );

        // LFS files should not have line counts (same as binary)
        assert!(metadata_diff.lines_count.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_lfs_file_creation(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo_with_lfs(&ctx).await?;

        // Create a base commit without the LFS file, then add it
        let base_cs = CreateCommitContext::new_root(&ctx, &repo).commit().await?;

        let other_cs = CreateCommitContext::new(&ctx, &repo, vec![base_cs])
            .add_file_with_type_and_lfs(
                "new_lfs_file.bin",
                b"binary\x00lfs\x01new".as_slice(),
                FileType::Regular,
                GitLfs::canonical_pointer(),
            )
            .commit()
            .await?;

        let base_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: base_cs,
            path: create_non_root_path("new_lfs_file.bin")?,
            replacement_path: None,
        });
        let other_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: other_cs,
            path: create_non_root_path("new_lfs_file.bin")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            Some((base_input, &repo)),
            Some((other_input, &repo)),
            false,
        )
        .await?;

        // Base side doesn't exist
        assert_eq!(metadata_diff.base_file_info.content_type, None);
        assert_eq!(metadata_diff.base_file_info.file_type, None);

        // Other side should be LfsPointer
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::LfsPointer)
        );
        assert_eq!(
            metadata_diff.other_file_info.file_type,
            Some(DiffFileType::Regular)
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_lfs_vs_non_lfs(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo_with_lfs(&ctx).await?;

        // Create a commit with both an LFS file and a normal text file
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file_with_type_and_lfs(
                "lfs_file.bin",
                b"binary\x00lfs\x01content".as_slice(),
                FileType::Regular,
                GitLfs::canonical_pointer(),
            )
            .add_file("normal_file.txt", "just plain text\n")
            .commit()
            .await?;

        // Check LFS file is reported as LfsPointer
        let lfs_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: cs,
            path: create_non_root_path("lfs_file.bin")?,
            replacement_path: None,
        });

        let lfs_metadata = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((lfs_input, &repo)),
            false,
        )
        .await?;
        assert_eq!(
            lfs_metadata.other_file_info.content_type,
            Some(DiffContentType::LfsPointer)
        );

        // Check normal file is still reported as Text
        let text_input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: cs,
            path: create_non_root_path("normal_file.txt")?,
            replacement_path: None,
        });

        let text_metadata = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((text_input, &repo)),
            false,
        )
        .await?;
        assert_eq!(
            text_metadata.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_lfs_content_input(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        let repo = init_test_repo(&ctx).await?;

        // Create a commit to get a valid ContentId in the blobstore
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file.txt", "some text content\n")
            .commit()
            .await?;

        // Get the content ID from the changeset
        let changeset = cs
            .load(&ctx, repo.repo_blobstore())
            .await
            .map_err(DiffError::internal)?;
        let content_id = match changeset
            .file_changes_map()
            .get(&create_non_root_path("file.txt")?)
        {
            Some(mononoke_types::FileChange::Change(tracked)) => tracked.content_id(),
            _ => panic!("Expected file change"),
        };

        // Test with Content input that has lfs_pointer set — should report LfsPointer
        let lfs_input = DiffSingleInput::Content(DiffInputContent {
            content_id,
            path: Some(create_non_root_path("file.txt")?),
            lfs_pointer: Some(DiffLfsPointer {
                sha256: "abcdef1234567890".to_string(),
                size: 1024,
            }),
        });

        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((lfs_input, &repo)),
            false,
        )
        .await?;

        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::LfsPointer)
        );

        // Test with Content input without lfs_pointer — should report Text
        let text_input = DiffSingleInput::Content(DiffInputContent {
            content_id,
            path: Some(create_non_root_path("file.txt")?),
            lfs_pointer: None,
        });

        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((text_input, &repo)),
            false,
        )
        .await?;

        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Text)
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_metadata_lfs_no_lfs_config(fb: FacebookInit) -> Result<(), DiffError> {
        let ctx = CoreContext::test_mock(fb);
        // Use a repo WITHOUT LFS enabled
        let repo = init_test_repo(&ctx).await?;

        // Create a commit with an LFS-marked file in a non-LFS repo.
        // Without git_lfs_interpret_pointers, get_lfs_pointer() returns None
        // so the content type falls through to normal analysis (Binary in this case).
        let cs = CreateCommitContext::new_root(&ctx, &repo)
            .add_file_with_type_and_lfs(
                "file.bin",
                b"binary\x00content".as_slice(),
                FileType::Regular,
                GitLfs::canonical_pointer(),
            )
            .commit()
            .await?;

        let input = DiffSingleInput::ChangesetPath(DiffInputChangesetPath {
            changeset_id: cs,
            path: create_non_root_path("file.bin")?,
            replacement_path: None,
        });

        let metadata_diff = metadata(
            &ctx,
            None::<(DiffSingleInput, &BasicTestRepo)>,
            Some((input, &repo)),
            false,
        )
        .await?;

        // Without LFS config, should fall through to Binary, not LfsPointer
        assert_eq!(
            metadata_diff.other_file_info.content_type,
            Some(DiffContentType::Binary)
        );

        Ok(())
    }
}
