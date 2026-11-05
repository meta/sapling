/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Find the commit that last changed a path.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use commit_id_types::CommitIdArgs;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;
use crate::args::commit_id::SchemeArgs;
use crate::args::commit_id::map_commit_ids;
use crate::args::commit_id::resolve_commit_id;
use crate::args::path::PathArgs;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::library::commit_id::render_commit_id;
use crate::render::Render;

#[derive(Parser)]
/// Find the commit that last changed a path
///
/// If nothing exists at the path but something did, this is the commit that
/// deleted it.
pub(super) struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    scheme_args: SchemeArgs,
    #[clap(flatten)]
    commit_id_args: CommitIdArgs,
    #[clap(flatten)]
    path_args: PathArgs,
    #[clap(long)]
    /// Also show the Perforce revision the path has at the commit (Perforce
    /// mirror repos only)
    p4: bool,
}

#[derive(Serialize)]
struct LastChangedOutput {
    #[serde(skip)]
    requested: String,
    exists: bool,
    ids: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p4_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p4_changelist: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p4_path: Option<String>,
}

impl Render for LastChangedOutput {
    type Args = CommandArgs;

    fn render(&self, args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        let schemes = args.scheme_args.scheme_string_set();
        render_commit_id(None, "\n", &self.requested, &self.ids, &schemes, w)?;
        writeln!(w)?;
        if !self.exists {
            writeln!(w, "deleted")?;
        }
        if let (Some(revision), Some(changelist)) = (self.p4_revision, self.p4_changelist) {
            let path = self.p4_path.as_deref().unwrap_or(&self.requested);
            writeln!(w, "{path}#{revision} change {changelist}")?;
        } else if args.p4 {
            writeln!(w, "no Perforce revision recorded")?;
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
    let commit_id = args.commit_id_args.clone().into_commit_id();
    let id = resolve_commit_id(&conn, &repo, &commit_id).await?;
    let commit = thrift::CommitSpecifier {
        repo: repo.clone(),
        id,
        ..Default::default()
    };
    let path = args.path_args.path.clone();
    let commit_and_path = thrift::CommitPathSpecifier {
        commit,
        path: path.clone(),
        ..Default::default()
    };
    let params = thrift::CommitPathLastChangedParams {
        identity_schemes: args.scheme_args.clone().into_request_schemes(),
        include_p4_revision: args.p4,
        ..Default::default()
    };
    let response = conn
        .commit_path_last_changed(&commit_and_path, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;

    let Some(last_change) = response.last_change else {
        bail!("{path} never existed");
    };
    let output = LastChangedOutput {
        requested: path,
        exists: last_change.exists,
        ids: map_commit_ids(last_change.last_changed_commit.values()),
        p4_revision: last_change.p4_revision,
        p4_changelist: last_change.p4_changelist,
        p4_path: last_change.p4_path,
    };
    app.target.render_one(&args, output).await
}
