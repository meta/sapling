/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Check restrictions on all paths changed in a commit.

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use commit_id_types::CommitIdArgs;
use scs_client_raw::thrift;

#[cfg(test)]
mod compact;
use serde::Serialize;

use super::PathRestrictionInfo;
use super::convert_restriction_roots;
use super::path_coverage_to_string;
use super::render_restriction_root_line;
use crate::ScscApp;
use crate::args::commit_id::resolve_commit_id;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::render::Render;

/// Query restriction information for all file changes in the specified commit.
#[derive(Parser)]
pub(super) struct ChangesArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    commit_id_args: CommitIdArgs,
}

#[derive(Serialize)]
struct ChangesOutput {
    are_restricted: String,
    has_access: String,
    changed_paths: Vec<String>,
    restriction_roots: Vec<PathRestrictionInfo>,
    authorized_paths: Vec<String>,
}

impl Render for ChangesOutput {
    type Args = ();

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        writeln!(w, "Restricted: {}", self.are_restricted)?;
        writeln!(w, "Has access: {}", self.has_access)?;

        if !self.changed_paths.is_empty() {
            writeln!(w, "\nChanged paths:")?;
            for path in &self.changed_paths {
                writeln!(w, "  {path}")?;
            }
        }

        if !self.restriction_roots.is_empty() {
            writeln!(w, "\nRestriction roots:")?;
            for info in &self.restriction_roots {
                writeln!(w, "  {}:", info.path)?;
                for root in &info.roots {
                    render_restriction_root_line(w, root)?;
                }
            }
        }

        if !self.authorized_paths.is_empty() {
            writeln!(w, "\nAuthorized paths:")?;
            for path in &self.authorized_paths {
                writeln!(w, "  {path}")?;
            }
        }

        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

pub(super) async fn run(app: ScscApp, args: ChangesArgs) -> Result<()> {
    let repo = args.repo_args.clone().into_repo_specifier();
    let commit_id = args.commit_id_args.clone().into_commit_id();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let id = resolve_commit_id(&conn, &repo, &commit_id).await?;

    let commit = thrift::CommitSpecifier {
        repo: repo.clone(),
        id,
        ..Default::default()
    };

    let params = thrift::CommitRestrictedPathsChangesParams {
        ..Default::default()
    };

    let response = conn
        .commit_restricted_paths_changes(&commit, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;

    let output = ChangesOutput {
        are_restricted: path_coverage_to_string(response.are_restricted),
        has_access: path_coverage_to_string(response.has_access),
        changed_paths: response.changed_paths,
        restriction_roots: convert_restriction_roots(response.restriction_roots),
        authorized_paths: response.authorized_paths,
    };

    app.target.render_one(&(), output).await
}
