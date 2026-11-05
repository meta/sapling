/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Find the commit a Perforce revision of a path was imported as.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;
use crate::args::commit_id::SchemeArgs;
use crate::args::commit_id::map_commit_ids;
use crate::args::path::PathArgs;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::library::commit_id::render_commit_id;
use crate::render::Render;

#[derive(Parser)]
/// Find the commit a Perforce revision of a path was imported as
///
/// Searches the path's history back from a bookmark of a Perforce mirror repo.
pub(super) struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    scheme_args: SchemeArgs,
    #[clap(flatten)]
    path_args: PathArgs,
    #[clap(long, short = 'r')]
    /// The Perforce revision of the path (the N of path#N)
    revision: i64,
    #[clap(long, short = 'B', default_value = "master")]
    /// The bookmark whose history is searched
    bookmark: String,
}

#[derive(Serialize)]
struct ResolveP4RevisionOutput {
    #[serde(skip)]
    requested: String,
    #[serde(skip)]
    revision: i64,
    found: bool,
    exists: bool,
    ids: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changelist: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_revision_walked: Option<i64>,
    commits_walked: i64,
}

impl Render for ResolveP4RevisionOutput {
    type Args = CommandArgs;

    fn render(&self, args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        if !self.found {
            match (self.head_revision, self.oldest_revision_walked) {
                (None, _) => bail!("{} never existed at {}", self.requested, args.bookmark),
                (Some(head), _) if self.revision > head => bail!(
                    "{}#{} is newer than {}, which has #{head}{}",
                    self.requested,
                    self.revision,
                    args.bookmark,
                    if self.head_exists == Some(false) {
                        " (deleted)"
                    } else {
                        ""
                    }
                ),
                (Some(_), Some(oldest)) => bail!(
                    "{}#{} not reached: gave up after {} changes at #{oldest}",
                    self.requested,
                    self.revision,
                    self.commits_walked
                ),
                (Some(_), None) => bail!(
                    "{}#{} is not in the history of {}",
                    self.requested,
                    self.revision,
                    args.bookmark
                ),
            }
        }
        let schemes = args.scheme_args.scheme_string_set();
        render_commit_id(None, "\n", &self.requested, &self.ids, &schemes, w)?;
        writeln!(w)?;
        if let Some(changelist) = self.changelist {
            let path = self.path.as_deref().unwrap_or(&self.requested);
            let action = if self.exists { "" } else { " (deleted)" };
            writeln!(w, "{path}#{} change {changelist}{action}", self.revision)?;
        }
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

pub(super) async fn run(app: ScscApp, args: CommandArgs) -> Result<()> {
    let repo = args.repo_args.clone().into_repo_specifier();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let path = args.path_args.path.clone();
    let params = thrift::RepoResolveP4FileRevisionParams {
        path: path.clone(),
        revision: args.revision,
        bookmark: Some(args.bookmark.clone()),
        identity_schemes: args.scheme_args.clone().into_request_schemes(),
        ..Default::default()
    };
    let response = conn
        .repo_resolve_p4_file_revision(&repo, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;

    let output = ResolveP4RevisionOutput {
        requested: path,
        revision: args.revision,
        found: response.commit.is_some(),
        exists: response.exists,
        ids: response
            .commit
            .as_ref()
            .map(|commit| map_commit_ids(commit.values()))
            .unwrap_or_default(),
        changelist: response.changelist,
        path: response.path,
        head_revision: response.head_revision,
        head_exists: response.head_exists,
        oldest_revision_walked: response.oldest_revision_walked,
        commits_walked: response.commits_walked,
    };
    app.target.render_one(&args, output).await
}
