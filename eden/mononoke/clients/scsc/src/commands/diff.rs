/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Diff two commits

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::Write;

use anyhow::Result;
use anyhow::bail;
use clap::ValueEnum;
use commit_id_types::CommitIdsArgs;
use futures::stream;
use maplit::btreeset;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;
use crate::args::commit_id::SchemeArgs;
use crate::args::commit_id::map_commit_ids;
use crate::args::commit_id::resolve_commit_ids;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::library::commit_id::render_commit_id;
use crate::library::diff::diff_files;
use crate::render::Render;

#[derive(ValueEnum, Clone, Copy, Debug)]
enum DiffFormat {
    RawDiff,
    MetadataDiff,
}

#[derive(clap::Parser)]
/// Diff files between two commits
pub(super) struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    commit_ids_args: CommitIdsArgs,
    #[clap(long, short)]
    /// Paths to diff.
    path: Option<Vec<String>>,
    #[clap(long)]
    /// Show copies/moves as adds/deletes.
    skip_copies_renames: bool,
    #[clap(long)]
    /// Compare against the source of subtree copies.
    compare_with_subtree_copy_sources: bool,
    #[clap(long)]
    /// Only list differing paths instead of printing the diff.
    paths_only: bool,
    #[clap(long, conflicts_with = "paths_only")]
    /// Instead of generating a real diff let's generate a placeholder diff that just says that file differs.
    placeholders_only: bool,
    #[clap(long, short = 'O')]
    /// Generate diff in repository order.
    ordered: bool,
    #[clap(long, requires = "ordered")]
    /// Generate ordered diff after a given path.
    after: Option<String>,
    #[clap(long, short, default_value_t = 100, requires = "ordered")]
    /// Generate ordered diff for at most LIMIT files.
    limit: usize,
    #[clap(long, short = 's')]
    /// Limit the total size in bytes of returned diffs.
    diff_size_limit: Option<i64>,
    /// The format of the diff.
    #[clap(long, short = 'f', value_enum, default_value_t = DiffFormat::RawDiff)]
    diff_format: DiffFormat,
    /// Number of lines of unified context around differences.
    #[clap(long = "unified", short = 'U', default_value_t = 3)]
    context: i64,
    #[clap(flatten)]
    scheme_args: SchemeArgs,
    #[clap(long)]
    /// Follow mutable overrides to the history that make it more user friendly and 'correct'
    follow_mutable_history: bool,
    #[clap(long)]
    /// List the exact omitted and downgraded paths in the partial-response
    /// note (opts the request into partial responses)
    include_omitted_paths: bool,
}

#[derive(Serialize)]
struct PathsOnlyOutput {
    files: Vec<thrift::CommitCompareFile>,
}

impl Render for PathsOnlyOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        for file in &self.files {
            match (file.base_file.as_ref(), file.other_file.as_ref()) {
                // The letters as in git-status
                (Some(base_file), None) => writeln!(w, "A {}", base_file.path)?,
                (None, Some(other_file)) => writeln!(w, "D {}", other_file.path)?,
                (Some(base_file), Some(other_file)) => match file.copy_info {
                    thrift::CopyInfo::NONE => writeln!(w, "M {}", base_file.path)?,
                    thrift::CopyInfo::MOVE => {
                        writeln!(w, "R {} -> {}", other_file.path, base_file.path)?;
                    }
                    thrift::CopyInfo::COPY => {
                        writeln!(w, "C {} -> {}", other_file.path, base_file.path)?;
                    }
                    // There is no more possibilities but thrift doesn't know about it.
                    _ => bail!("unrecognized CopyInfo!"),
                },
                // Also rather impossible
                (None, None) => bail!("empty file pair received!"),
            }
        }
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

#[derive(Serialize)]
struct SubtreeChangeOutput {
    change_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_commit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_commit_ids: Option<BTreeMap<String, String>>,
    source_path: String,
    destination_path: String,
    #[serde(skip)]
    requested: String,
    #[serde(skip)]
    schemes: HashSet<String>,
}

impl SubtreeChangeOutput {
    fn from_thrift(
        requested: String,
        schemes: HashSet<String>,
        path: String,
        change: thrift::SubtreeChange,
    ) -> Option<Self> {
        match change {
            thrift::SubtreeChange::subtree_copy(copy) => Some(SubtreeChangeOutput {
                change_type: String::from("copy"),
                source_url: None,
                source_commit_id: None,
                source_commit_ids: Some(map_commit_ids(copy.source_commit_ids.values())),
                source_path: copy.source_path.clone(),
                destination_path: path,
                requested: requested.clone(),
                schemes: schemes.clone(),
            }),
            thrift::SubtreeChange::subtree_merge(merge) => Some(SubtreeChangeOutput {
                change_type: String::from("merge"),
                source_url: None,
                source_commit_id: None,
                source_commit_ids: Some(map_commit_ids(merge.source_commit_ids.values())),
                source_path: merge.source_path.clone(),
                destination_path: path,
                requested: requested.clone(),
                schemes: schemes.clone(),
            }),
            thrift::SubtreeChange::subtree_import(import) => Some(SubtreeChangeOutput {
                change_type: String::from("import"),
                source_url: Some(import.source_url.clone()),
                source_commit_id: Some(import.source_commit_id.clone()),
                source_commit_ids: None,
                source_path: import.source_path.clone(),
                destination_path: path,
                requested: requested.clone(),
                schemes: schemes.clone(),
            }),
            thrift::SubtreeChange::subtree_cross_repo_merge(xmerge) => Some(SubtreeChangeOutput {
                change_type: String::from("cross_repo_merge"),
                source_url: Some(xmerge.source_url.clone()),
                source_commit_id: Some(xmerge.source_commit_id.clone()),
                source_commit_ids: None,
                source_path: xmerge.source_path.clone(),
                destination_path: path,
                requested: requested.clone(),
                schemes: schemes.clone(),
            }),
            _ => None,
        }
    }
}

impl Render for SubtreeChangeOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        write!(w, "subtree {} ", self.change_type)?;
        if let Some(source_url) = &self.source_url {
            write!(w, "{source_url} ")?;
        }
        if let Some(source_commit_id) = &self.source_commit_id {
            write!(w, "{source_commit_id}")?;
        }
        if let Some(source_commit_ids) = &self.source_commit_ids {
            render_commit_id(
                None,
                ",",
                &self.requested,
                source_commit_ids,
                &self.schemes,
                w,
            )?;
        }
        writeln!(w, " {} {}", self.source_path, self.destination_path)?;
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

fn compare_file_path(file: &thrift::CommitCompareFile) -> &str {
    file.base_file
        .as_ref()
        .or(file.other_file.as_ref())
        .map_or("", |f| f.path.as_str())
}

pub(super) async fn run(app: ScscApp, args: CommandArgs) -> Result<()> {
    let repo = args.repo_args.clone().into_repo_specifier();
    let commit_ids = args.commit_ids_args.clone().into_commit_ids();
    if commit_ids.len() > 2 || commit_ids.is_empty() {
        bail!("expected 1 or 2 commit_ids (got {})", commit_ids.len())
    }
    let paths = args.path.clone();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let commit_ids = resolve_commit_ids(&conn, &repo, &commit_ids).await?;
    let identity_schemes = args.scheme_args.clone().into_request_schemes();

    let commits: Vec<_> = commit_ids
        .iter()
        .map(|id| thrift::CommitSpecifier {
            repo: repo.clone(),
            id: id.clone(),
            ..Default::default()
        })
        .collect();
    let (other_commit, base_commit) = if commits.len() == 1 {
        (None, commits[0].clone())
    } else {
        (commits.first(), commits[1].clone())
    };

    if args.compare_with_subtree_copy_sources {
        // Fetch and display any subtree copy sources
        let subtree_changes = conn
            .commit_subtree_changes(
                &base_commit,
                &thrift::CommitSubtreeChangesParams {
                    identity_schemes: identity_schemes.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.handle_selection_error(&repo))?
            .subtree_changes;

        let requested = commit_ids.last().unwrap().to_string();
        let schemes = args.scheme_args.scheme_string_set();

        app.target
            .render(
                &args,
                stream::iter(
                    subtree_changes
                        .into_iter()
                        .filter_map(|(path, change)| {
                            SubtreeChangeOutput::from_thrift(
                                requested.clone(),
                                schemes.clone(),
                                path,
                                change,
                            )
                        })
                        .map(Ok),
                ),
            )
            .await?;
    }

    let ordered_params = if args.ordered {
        let after_path = args.after.clone();
        let limit: i64 = args.limit.try_into()?;
        Some(thrift::CommitCompareOrderedParams {
            after_path,
            limit,
            ..Default::default()
        })
    } else {
        None
    };
    let diff_format = match args.diff_format {
        DiffFormat::RawDiff => thrift::DiffFormat::RAW_DIFF,
        DiffFormat::MetadataDiff => thrift::DiffFormat::METADATA_DIFF,
    };

    let params = thrift::CommitCompareParams {
        other_commit_id: other_commit.map(|c| c.id.clone()),
        skip_copies_renames: args.skip_copies_renames,
        identity_schemes,
        paths,
        compare_items: btreeset! {thrift::CommitCompareItem::FILES},
        ordered_params,
        compare_with_subtree_copy_sources: Some(args.compare_with_subtree_copy_sources),
        follow_mutable_file_history: Some(args.follow_mutable_history),
        partial_options: crate::util::partial_options(args.include_omitted_paths),
        ..Default::default()
    };
    let mut response = conn
        .commit_compare(&base_commit, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;

    response.diff_files.sort_by(|a, b| {
        compare_file_path(a)
            .split('/')
            .cmp(compare_file_path(b).split('/'))
    });
    let compare_partial = crate::util::is_partial_verdict(&response.partial_info);

    if args.paths_only {
        let result = app
            .target
            .render_one(
                &args,
                PathsOnlyOutput {
                    files: response.diff_files,
                },
            )
            .await;
        if result.is_ok() && compare_partial {
            crate::util::print_partial_note(crate::util::partial_groups(&response.partial_info));
        }
        return result;
    }

    // Use original ephemeral_bonsai commit IDs to preserve bubble info for snapshots.
    // For other cases (including subtree operations), use response.other_commit_ids.
    let other_commit_id = match other_commit {
        Some(c) if matches!(c.id, thrift::CommitId::ephemeral_bonsai(_)) => Some(c.id.clone()),
        _ => response
            .other_commit_ids
            .and_then(|ids| ids.into_iter().next().map(|(_scheme, id)| id)),
    };

    let placeholder_only = args.placeholders_only;
    let paths_sizes = response.diff_files.iter().map(|diff_file| {
        let pair_size = diff_file.base_file.as_ref().map_or(0, |f| f.info.file_size)
            + diff_file
                .other_file
                .as_ref()
                .map_or(0, |f| f.info.file_size);
        (
            thrift::CommitFileDiffsParamsPathPair {
                base_path: diff_file.base_file.as_ref().map(|f| f.path.clone()),
                other_path: diff_file.other_file.as_ref().map(|f| f.path.clone()),
                copy_info: diff_file.copy_info,
                generate_placeholder_diff: Some(
                    placeholder_only || pair_size > thrift::COMMIT_FILE_DIFFS_SIZE_LIMIT,
                ),
                subtree_source: diff_file.subtree_source.as_ref().map(|source| {
                    thrift::CommitFileDiffsParamsSubtreeSource {
                        path: source.source_path.clone(),
                        commit_id: source
                            .source_commit_ids
                            .values()
                            .next()
                            .expect("expected commit id")
                            .clone(),
                        ..Default::default()
                    }
                }),
                ..Default::default()
            },
            pair_size,
        )
    });
    let result = app
        .target
        .render(
            &(),
            diff_files(
                &conn,
                base_commit.clone(),
                other_commit_id,
                paths_sizes,
                args.diff_size_limit,
                diff_format,
                args.context,
                args.include_omitted_paths,
            ),
        )
        .await;
    if result.is_ok() && (compare_partial || conn.is_partial()) {
        let mut groups = crate::util::partial_groups(&response.partial_info);
        groups.extend(conn.partial_groups());
        crate::util::print_partial_note(groups);
    }
    result
}
