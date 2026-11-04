/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Compact restriction comparisons, fetched page by page from immutable endpoints.

use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use clap::Parser;
use commit_id_types::CommitIdsArgs;
use scs_client_raw::thrift;

use crate::ScscApp;
use crate::args::commit_id::resolve_commit_ids;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::render::OutputFormat;

mod compact;

#[derive(Parser)]
pub(super) struct ChangesArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    commit_ids_args: CommitIdsArgs,
    /// Maximum paths per request. Without --stream, every page is consumed
    /// before rendering.
    #[clap(long, default_value_t = thrift::COMMIT_RESTRICTED_PATHS_CHANGES_MAX_LIMIT,
        value_parser = clap::value_parser!(i64).range(1..=thrift::COMMIT_RESTRICTED_PATHS_CHANGES_MAX_LIMIT))]
    page_size: i64,
    /// Emit one JSON line per validated page, with page-local policy and
    /// source tables, then a line with `"complete": true` and the verdicts.
    /// Requires --json. Without that final line the output is incomplete,
    /// and a failure after some pages still exits non-zero.
    #[clap(long)]
    stream: bool,
}

fn comparison_commits(
    ids: &[thrift::CommitId],
) -> Result<(thrift::CommitId, Option<thrift::CommitId>)> {
    match ids {
        [target] => Ok((target.clone(), None)),
        [base, target] => Ok((target.clone(), Some(base.clone()))),
        _ => bail!("expected one target commit or a base and target commit"),
    }
}

fn comparison_params(
    base: Option<&thrift::CommitId>,
    after: Option<&str>,
    limit: i64,
) -> thrift::CommitRestrictedPathsChangesParams {
    thrift::CommitRestrictedPathsChangesParams {
        other_commit_id: base.cloned(),
        ordered_params: Some(thrift::CommitRestrictedPathsChangesOrderedParams {
            after_path: after.map(str::to_owned),
            limit,
            ..Default::default()
        }),
        format: Some(thrift::CommitRestrictedPathsChangesFormat::COMPACT),
        ..Default::default()
    }
}

pub(super) async fn run(app: ScscApp, args: ChangesArgs) -> Result<()> {
    ensure!(
        !args.stream || matches!(app.target, OutputFormat::Json),
        "--stream requires --json"
    );
    let repo = args.repo_args.clone().into_repo_specifier();
    let commit_ids = args.commit_ids_args.into_commit_ids();
    ensure!(
        (1..=2).contains(&commit_ids.len()),
        "expected one target commit or a base and target commit"
    );
    let conn = app.get_connection(Some(&repo.name)).await?;
    let ids = resolve_commit_ids(&conn, &repo, &commit_ids).await?;
    let (id, base) = comparison_commits(&ids)?;
    let commit = thrift::CommitSpecifier {
        repo: repo.clone(),
        id,
        ..Default::default()
    };
    let (conn, commit, repo, base) = (&conn, &commit, &repo, base.as_ref());
    let page_size = args.page_size;
    let fetch = move |after: Option<String>| async move {
        let params = comparison_params(base, after.as_deref(), page_size);
        conn.commit_restricted_paths_changes(commit, &params)
            .await
            .map_err(|error| error.handle_selection_error(repo))
    };
    if args.stream {
        return app
            .target
            .render(&(), compact::stream_frames(page_size, fetch))
            .await;
    }
    let output = compact::complete(page_size, fetch).await?;
    app.target.render_one(&(), output).await
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn fixed_endpoints_and_compact_params() -> Result<()> {
        let base = thrift::CommitId::bonsai(vec![1; 32]);
        let target = thrift::CommitId::bonsai(vec![2; 32]);
        assert_eq!(
            comparison_commits(std::slice::from_ref(&target))?,
            (target.clone(), None)
        );
        assert_eq!(
            comparison_commits(&[base.clone(), target.clone()])?,
            (target, Some(base.clone()))
        );
        assert!(comparison_commits(&[]).is_err());
        assert!(comparison_commits(&[base.clone(), base.clone(), base.clone()]).is_err());
        let params = comparison_params(Some(&base), Some("a/b"), 2);
        assert_eq!(params.other_commit_id, Some(base));
        assert_eq!(
            params.format,
            Some(thrift::CommitRestrictedPathsChangesFormat::COMPACT)
        );
        let ordered = params.ordered_params.expect("compact is paged");
        assert_eq!(ordered.after_path.as_deref(), Some("a/b"));
        assert_eq!(ordered.limit, 2);
        Ok(())
    }
}
