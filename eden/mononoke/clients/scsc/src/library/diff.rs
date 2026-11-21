/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Helper library for returning file diffs

use std::io::Write;

use anyhow::Result;
use cloned::cloned;
use futures::Stream;
use futures::stream;
use futures::stream::StreamExt;
use scs_client_raw::ScsClient;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::render::Render;

#[derive(Serialize)]
struct FileInfo {
    file_type: Option<String>,
    file_content_type: Option<String>,
    file_generated_status: Option<String>,
}

impl From<&thrift::MetadataDiffFileInfo> for FileInfo {
    fn from(file_info: &thrift::MetadataDiffFileInfo) -> Self {
        Self {
            file_type: file_info
                .file_type
                .as_ref()
                .map(thrift::MetadataDiffFileType::to_string),
            file_content_type: file_info
                .file_content_type
                .as_ref()
                .map(thrift::MetadataDiffFileContentType::to_string),
            file_generated_status: file_info
                .file_generated_status
                .as_ref()
                .map(thrift::FileGeneratedStatus::to_string),
        }
    }
}

#[derive(Serialize)]
struct LinesCount {
    added_lines_count: i64,
    deleted_lines_count: i64,
    significant_added_lines_count: i64,
    significant_deleted_lines_count: i64,
    first_added_line_number: Option<i64>,
}

impl From<&thrift::MetadataDiffLinesCount> for LinesCount {
    fn from(lines_count: &thrift::MetadataDiffLinesCount) -> Self {
        Self {
            added_lines_count: lines_count.added_lines_count,
            deleted_lines_count: lines_count.deleted_lines_count,
            significant_added_lines_count: lines_count.significant_added_lines_count,
            significant_deleted_lines_count: lines_count.significant_deleted_lines_count,
            first_added_line_number: lines_count.first_added_line_number,
        }
    }
}

#[derive(Serialize)]
struct MetadataDiffElement {
    old_path: String,
    new_path: String,
    old_file_info: FileInfo,
    new_file_info: FileInfo,
    lines_count: Option<LinesCount>,
}

#[derive(Serialize)]
enum DiffOutputElement {
    RawDiff(Vec<u8>),
    MetadataDiff(MetadataDiffElement),
}

#[derive(Serialize)]
struct DiffOutput {
    diffs: Vec<DiffOutputElement>,
    stopped_at_pair: Option<thrift::CommitFileDiffsStoppedAtPair>,
}

fn render_file_info(
    tag: &str,
    path: &String,
    file_info: &FileInfo,
    w: &mut dyn Write,
) -> Result<()> {
    write!(w, "{tag} {path},")?;
    if let Some(file_type) = &file_info.file_type {
        write!(w, " file type: {file_type},")?;
    }
    if let Some(content_type) = &file_info.file_content_type {
        write!(w, " content type: {content_type},")?;
    }
    if let Some(generated_status) = &file_info.file_generated_status {
        write!(w, " generated status: {generated_status},")?;
    }
    write!(w, "\n")?;
    Ok(())
}

impl Render for DiffOutput {
    type Args = ();

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        for diff in &self.diffs {
            match diff {
                DiffOutputElement::RawDiff(diff) => write!(w, "{}", String::from_utf8_lossy(diff))?,
                DiffOutputElement::MetadataDiff(diff) => {
                    render_file_info("---", &diff.old_path, &diff.old_file_info, w)?;
                    render_file_info("+++", &diff.new_path, &diff.new_file_info, w)?;

                    if let Some(lines_count) = &diff.lines_count {
                        if let Some(first_added_line_number) = lines_count.first_added_line_number {
                            write!(w, "first added line number: {first_added_line_number}, ")?;
                        }
                        writeln!(
                            w,
                            "{} significant lines ({} added, {} deleted), {} total ({} added, {} deleted)",
                            lines_count.significant_added_lines_count
                                + lines_count.significant_deleted_lines_count,
                            lines_count.significant_added_lines_count,
                            lines_count.significant_deleted_lines_count,
                            lines_count.added_lines_count + lines_count.deleted_lines_count,
                            lines_count.added_lines_count,
                            lines_count.deleted_lines_count,
                        )?
                    }
                    write!(w, "\n")?;
                }
            }
        }

        if let Some(stopped_at_pair) = &self.stopped_at_pair {
            writeln!(
                w,
                "stopped at (base_path: {}, other_path: {})",
                stopped_at_pair
                    .base_path
                    .as_ref()
                    .unwrap_or(&String::from("")),
                stopped_at_pair
                    .other_path
                    .as_ref()
                    .unwrap_or(&String::from(""))
            )?;
        }

        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

async fn make_file_diff_request(
    connection: &ScsClient,
    commit: &thrift::CommitSpecifier,
    other_commit_id: Option<thrift::CommitId>,
    paths: Vec<thrift::CommitFileDiffsParamsPathPair>,
    diff_size_limit: Option<i64>,
    diff_format: thrift::DiffFormat,
    context: i64,
    include_omitted_paths: bool,
) -> Result<DiffOutput> {
    let params = thrift::CommitFileDiffsParams {
        other_commit_id,
        paths,
        format: diff_format,
        context,
        diff_size_limit,
        partial_options: crate::util::partial_options(include_omitted_paths),
        ..Default::default()
    };

    let response = connection.commit_file_diffs(commit, &params).await?;
    if crate::util::is_partial_verdict(&response.partial_info) {
        connection.note_partial(crate::util::partial_groups(&response.partial_info));
    }
    let diffs: Vec<_> = response
        .path_diffs
        .into_iter()
        .filter_map(|path_diff| match path_diff {
            thrift::CommitFileDiffsResponseElement {
                diff: Some(thrift::Diff::raw_diff(diff)),
                ..
            } => Some(DiffOutputElement::RawDiff(
                diff.raw_diff.unwrap_or_else(Vec::new),
            )),
            thrift::CommitFileDiffsResponseElement {
                diff: Some(thrift::Diff::metadata_diff(diff)),
                base_path,
                other_path,
                ..
            } => Some(DiffOutputElement::MetadataDiff(MetadataDiffElement {
                old_path: other_path
                    .map_or_else(|| "/dev/null".to_string(), |path| format!("a/{path}")),
                new_path: base_path
                    .map_or_else(|| "/dev/null".to_string(), |path| format!("b/{path}")),
                old_file_info: FileInfo::from(&diff.old_file_info),
                new_file_info: FileInfo::from(&diff.new_file_info),
                lines_count: diff.lines_count.as_ref().map(LinesCount::from),
            })),
            _ => None,
        })
        .collect();

    Ok(DiffOutput {
        diffs,
        stopped_at_pair: response.stopped_at_pair,
    })
}

/// Given the paths and sizes of files to diff returns the stream of renderable
/// structs. The sizes are used to avoid hitting size limit when doing batch requests.
/// A batch whose response was partial records it on the client, so the caller
/// can emit a single note after rendering succeeds.
pub(crate) fn diff_files<T: IntoIterator<Item = (thrift::CommitFileDiffsParamsPathPair, i64)>>(
    connection: &ScsClient,
    commit: thrift::CommitSpecifier,
    other_commit_id: Option<thrift::CommitId>,
    paths_sizes: T,
    diff_size_limit: Option<i64>,
    diff_format: thrift::DiffFormat,
    context: i64,
    include_omitted_paths: bool,
) -> impl Stream<Item = Result<impl Render<Args = ()> + use<T>>> + use<T> {
    let mut size_sum: i64 = 0;
    let mut path_count: i64 = 0;
    let mut paths = Vec::new();
    let mut requests = Vec::new();
    cloned!(connection);
    for (path, size) in paths_sizes {
        if size + size_sum > thrift::COMMIT_FILE_DIFFS_SIZE_LIMIT
            || path_count + 1 > thrift::COMMIT_FILE_DIFFS_PATH_COUNT_LIMIT
        {
            requests.push(paths);
            paths = Vec::new();
            size_sum = 0;
            path_count = 0;
        }
        paths.push(path);
        path_count += 1;
        size_sum += size;
    }
    requests.push(paths);
    stream::iter(requests).then(move |paths| {
        let connection = connection.clone();
        let commit = commit.clone();
        let other_commit_id = other_commit_id.clone();
        async move {
            make_file_diff_request(
                &connection,
                &commit,
                other_commit_id,
                paths,
                diff_size_limit,
                diff_format,
                context,
                include_omitted_paths,
            )
            .await
        }
    })
}
