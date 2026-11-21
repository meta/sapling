/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Display information about a commit, directory, or file.

use std::collections::HashSet;
use std::io::Write;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use commit_id_types::CommitIdArgs;
use futures::stream;
use futures::stream::StreamExt;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;
use crate::args::commit_id::SchemeArgs;
use crate::args::commit_id::resolve_commit_id;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::library::bookmark::BookmarkInfo;
use crate::library::bookmark::render_bookmark_info;
use crate::library::commit::CommitInfo;
use crate::library::commit::render_commit_info;
use crate::render::Render;
use crate::util::byte_count_iec;
use crate::util::plural;

#[derive(clap::Parser)]
/// Fetch info about a commit, directory, file or bookmark
pub(super) struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    scheme_args: SchemeArgs,
    #[clap(flatten)]
    commit_id_args: CommitIdArgs,
    #[clap(long, short, num_args = 1..)]
    /// Path or paths to display info for.  If not present, display info about
    /// the commit.  To display directory info about the root directory, use
    /// -p ""
    path: Option<Vec<String>>,
    #[clap(long, conflicts_with = "path")]
    /// Display info about bookmark itself rather than the commit it points to
    bookmark_info: bool,
    #[clap(long)]
    /// List the exact omitted and downgraded paths in the partial-response
    /// note (opts the request into partial responses)
    include_omitted_paths: bool,
}

struct CommitInfoOutput {
    commit: CommitInfo,
    requested: String,
    schemes: HashSet<String>,
}

impl Render for CommitInfoOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        render_commit_info(&self.commit, &self.requested, &self.schemes, w)
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, &self.commit)?)
    }
}

struct BookmarkInfoOutput {
    bookmark_info: BookmarkInfo,
    requested: String,
    schemes: HashSet<String>,
}

impl Render for BookmarkInfoOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        render_bookmark_info(&self.bookmark_info, &self.requested, &self.schemes, w)
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, &self.bookmark_info)?)
    }
}

#[derive(Serialize)]
struct TreeInfoOutput {
    path: String,
    r#type: String,
    id: String,
    child_files_count: i64,
    child_files_total_size: i64,
    child_dirs_count: i64,
    descendant_files_count: i64,
    descendant_files_total_size: i64,
}

impl Render for TreeInfoOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        write!(w, "Path: {}\n", self.path)?;
        write!(w, "Type: {}\n", self.r#type)?;
        write!(w, "Id: {}\n", self.id)?;
        write!(
            w,
            "Children: {} {} ({}), {} {}\n",
            self.child_files_count,
            plural(self.child_files_count, "file", "files"),
            byte_count_iec(self.child_files_total_size),
            self.child_dirs_count,
            plural(self.child_dirs_count, "dir", "dirs"),
        )?;
        write!(
            w,
            "Descendants: {} {} ({})\n",
            self.descendant_files_count,
            plural(self.descendant_files_count, "file", "files"),
            byte_count_iec(self.descendant_files_total_size)
        )?;
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

#[derive(Serialize)]
struct FileInfoOutput {
    path: String,
    r#type: String,
    size: i64,
    id: String,
    content_sha1: String,
    content_sha256: String,
}

impl Render for FileInfoOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        write!(w, "Path: {}\n", self.path)?;
        write!(w, "Type: {}\n", self.r#type)?;
        write!(w, "Id: {}\n", self.id)?;
        write!(w, "Content-SHA1: {}\n", self.content_sha1)?;
        write!(w, "Content-SHA256: {}\n", self.content_sha256)?;
        write!(w, "Size: {}\n", byte_count_iec(self.size))?;
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

async fn commit_info(app: ScscApp, args: CommandArgs, repo: thrift::RepoSpecifier) -> Result<()> {
    let commit_id = args.commit_id_args.clone().into_commit_id();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let id = resolve_commit_id(&conn, &repo, &commit_id).await?;
    let commit = thrift::CommitSpecifier {
        repo,
        id,
        ..Default::default()
    };
    let params = thrift::CommitInfoParams {
        identity_schemes: args.scheme_args.clone().into_request_schemes(),
        ..Default::default()
    };
    let response = conn
        .commit_info(&commit, &params)
        .await
        .map_err(|e| e.handle_selection_error(&commit.repo))?;

    let commit_info = CommitInfo::try_from(&response)?;
    let output = CommitInfoOutput {
        commit: commit_info,
        requested: commit_id.to_string(),
        schemes: args.scheme_args.scheme_string_set(),
    };
    app.target.render_one(&args, output).await
}

async fn bookmark_info(app: ScscApp, args: CommandArgs, repo: thrift::RepoSpecifier) -> Result<()> {
    let conn = app.get_connection(Some(&repo.name)).await?;
    let bookmark_name = args
        .commit_id_args
        .clone()
        .into_commit_id()
        .into_bookmark_name()?;
    let params = thrift::RepoBookmarkInfoParams {
        bookmark_name: bookmark_name.clone(),
        identity_schemes: args.scheme_args.clone().into_request_schemes(),
        ..Default::default()
    };
    let response = conn
        .repo_bookmark_info(&repo, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;
    let info = response
        .info
        .ok_or_else(|| anyhow!("Bookmark doesn't exist"))?;

    let info = BookmarkInfo::try_from(&info)?;
    let output = BookmarkInfoOutput {
        bookmark_info: info,
        requested: bookmark_name,
        schemes: args.scheme_args.scheme_string_set(),
    };
    app.target.render_one(&args, output).await
}

async fn path_info(
    app: ScscApp,
    args: CommandArgs,
    repo: thrift::RepoSpecifier,
    path: String,
) -> Result<()> {
    let commit_id = args.commit_id_args.clone().into_commit_id();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let id = resolve_commit_id(&conn, &repo, &commit_id).await?;
    let commit = thrift::CommitSpecifier {
        repo: repo.clone(),
        id,
        ..Default::default()
    };
    let commit_path = thrift::CommitPathSpecifier {
        commit,
        path: path.clone(),
        ..Default::default()
    };
    let params = thrift::CommitPathInfoParams {
        ..Default::default()
    };
    let response = conn
        .commit_path_info(&commit_path, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;
    if response.exists {
        match (response.r#type, response.info) {
            (Some(entry_type), Some(thrift::EntryInfo::tree(info))) => {
                app.target
                    .render_one(&args, tree_info(path, entry_type, info))
                    .await
            }
            (Some(entry_type), Some(thrift::EntryInfo::file(info))) => {
                app.target
                    .render_one(&args, file_info(path, entry_type, info))
                    .await
            }
            _ => {
                bail!("malformed response for '{path}' in {commit_id}")
            }
        }
    } else {
        bail!("'{path}' does not exist in {commit_id}")
    }
}

async fn multiple_path_info(
    app: ScscApp,
    args: CommandArgs,
    repo: thrift::RepoSpecifier,
    paths: Vec<String>,
) -> Result<()> {
    let commit_id = args.commit_id_args.clone().into_commit_id();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let id = resolve_commit_id(&conn, &repo, &commit_id).await?;
    let commit = thrift::CommitSpecifier {
        repo,
        id,
        ..Default::default()
    };
    let params = thrift::CommitMultiplePathInfoParams {
        paths,
        partial_options: crate::util::partial_options(args.include_omitted_paths),
        ..Default::default()
    };
    let response = conn
        .commit_multiple_path_info(&commit, &params)
        .await
        .map_err(|e| e.handle_selection_error(&commit.repo))?;

    let info_partial = crate::util::is_partial_verdict(&response.partial_info);
    let output = stream::iter(response.path_info).map(move |(path, commit_info)| {
        match (commit_info.r#type, commit_info.info) {
            (Some(entry_type), Some(thrift::EntryInfo::tree(info))) => {
                Ok(Box::new(tree_info(path, entry_type, info))
                    as Box<dyn Render<Args = CommandArgs>>)
            }
            (Some(entry_type), Some(thrift::EntryInfo::file(info))) => {
                Ok(Box::new(file_info(path, entry_type, info))
                    as Box<dyn Render<Args = CommandArgs>>)
            }
            _ => {
                bail!("malformed response for '{path}'");
            }
        }
    });
    let result = app.target.render(&args, output).await;
    if result.is_ok() && info_partial {
        crate::util::print_partial_note(crate::util::partial_groups(&response.partial_info));
    }
    result
}

fn tree_info(
    path: String,
    entry_type: thrift::EntryType,
    info: thrift::TreeInfo,
) -> TreeInfoOutput {
    let id = faster_hex::hex_string(&info.id);
    TreeInfoOutput {
        path,
        r#type: entry_type.to_string().to_lowercase(),
        id,
        child_files_count: info.child_files_count,
        child_dirs_count: info.child_dirs_count,
        child_files_total_size: info.child_files_total_size,
        descendant_files_count: info.descendant_files_count,
        descendant_files_total_size: info.descendant_files_total_size,
    }
}

fn file_info(
    path: String,
    entry_type: thrift::EntryType,
    info: thrift::FileInfo,
) -> FileInfoOutput {
    let id = faster_hex::hex_string(&info.id);
    let content_sha1 = faster_hex::hex_string(&info.content_sha1);
    let content_sha256 = faster_hex::hex_string(&info.content_sha256);
    FileInfoOutput {
        path,
        r#type: entry_type.to_string().to_lowercase(),
        id,
        content_sha1,
        content_sha256,
        size: info.file_size,
    }
}

pub(super) async fn run(app: ScscApp, args: CommandArgs) -> Result<()> {
    let repo = args.repo_args.clone().into_repo_specifier();

    match args.path.as_deref() {
        Some([path]) => {
            let path = path.clone();
            path_info(app, args, repo, path.clone()).await
        }
        Some(paths) => {
            let paths = paths.to_vec();
            multiple_path_info(app, args, repo, paths).await
        }
        None => {
            if args.bookmark_info {
                bookmark_info(app, args, repo).await
            } else {
                commit_info(app, args, repo).await
            }
        }
    }
}
