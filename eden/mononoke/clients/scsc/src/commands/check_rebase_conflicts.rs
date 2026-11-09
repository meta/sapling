/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use anyhow::bail;
use commit_id_types::CommitIdsArgs;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;
use crate::args::commit_id::SchemeArgs;
use crate::args::commit_id::map_commit_ids;
use crate::args::commit_id::resolve_commit_ids;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::library::commit_id::render_commit_id;
use crate::render::Render;

#[derive(clap::Parser)]
/// Check whether a draft stack would merge cleanly onto a bookmark
///
/// Provide the head of the stack and the bookmark to check against. The
/// stack base is the merge base of the head and the bookmark tip. Nothing
/// is created or moved.
pub(super) struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    scheme_args: SchemeArgs,
    #[clap(flatten)]
    commit_ids_args: CommitIdsArgs,
    #[clap(long)]
    /// Name of the bookmark to check the merge against
    target: String,
    /// Treat any path changed on both sides as a conflict instead of
    /// content-merging it
    #[clap(long)]
    no_merge: bool,
}

#[derive(Serialize)]
struct CheckRebaseConflictsOutput {
    status: String,
    conflicts: Vec<String>,
    target: BTreeMap<String, String>,
    overlapping_path_count: i64,
    merged_path_count: i64,
}

impl Render for CheckRebaseConflictsOutput {
    type Args = SchemeArgs;

    fn render(&self, args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        let schemes = args.scheme_string_set();
        write!(w, "{} onto", self.status)?;
        render_commit_id(
            Some(("", "    ")),
            "\n",
            "target",
            &self.target,
            &schemes,
            w,
        )?;
        write!(w, "\n")?;
        for conflict in self.conflicts.iter() {
            write!(w, "  {conflict}\n")?;
        }
        if self.overlapping_path_count > 0 {
            write!(
                w,
                "{} overlapping paths, {} merged\n",
                self.overlapping_path_count, self.merged_path_count
            )?;
        }
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

pub(super) async fn run(app: ScscApp, args: CommandArgs) -> Result<()> {
    let repo = args.repo_args.into_repo_specifier();
    let commit_ids = args.commit_ids_args.into_commit_ids();
    if commit_ids.len() != 1 {
        bail!("expected 1 commit_id (got {})", commit_ids.len())
    }
    let conn = app.get_connection(Some(&repo.name)).await?;
    let ids = resolve_commit_ids(&conn, &repo, &commit_ids).await?;
    let head = match ids.as_slice() {
        [head] => head.clone(),
        _ => bail!("expected 1 commit_id (got {})", ids.len()),
    };

    let params = thrift::RepoCheckRebaseConflictsParams {
        head,
        target_bookmark: args.target,
        identity_schemes: args.scheme_args.clone().into_request_schemes(),
        merge_resolution: if args.no_merge {
            thrift::RepoRebaseStackMergeResolution::DISABLED
        } else {
            thrift::RepoRebaseStackMergeResolution::DEFAULT
        },
        ..Default::default()
    };
    let response = conn
        .repo_check_rebase_conflicts(&repo, &params)
        .await
        .map_err(|e| e.handle_selection_error(&repo))?;
    let output = CheckRebaseConflictsOutput {
        status: match response.status {
            thrift::RepoCheckRebaseConflictsStatus::CLEAN => "clean".to_string(),
            thrift::RepoCheckRebaseConflictsStatus::CONFLICT => "conflict".to_string(),
            thrift::RepoCheckRebaseConflictsStatus::ALREADY_LANDED => "already_landed".to_string(),
            thrift::RepoCheckRebaseConflictsStatus::UNKNOWN => "unknown".to_string(),
            _ => "unknown".to_string(),
        },
        conflicts: response.conflicts.into_iter().map(|c| c.left).collect(),
        target: map_commit_ids(response.target_ids.values()),
        overlapping_path_count: response.overlapping_path_count,
        merged_path_count: response.merged_path_count,
    };
    app.target.render_one(&args.scheme_args, output).await
}
