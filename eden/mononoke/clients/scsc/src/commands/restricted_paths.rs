/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Commands for querying restricted paths in a repository.

mod access;
mod changes;
mod find;

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;

#[derive(Parser)]
/// Query restricted paths in a repository
pub(super) struct CommandArgs {
    #[command(subcommand)]
    subcommand: RestrictedPathsSubcommand,
}

#[derive(Subcommand)]
enum RestrictedPathsSubcommand {
    /// Check if specific paths are restricted and if the caller has access
    Access(access::AccessArgs),
    /// Find all restriction roots under specified paths
    Find(find::FindArgs),
    /// Check restrictions on all paths changed in a commit
    Changes(changes::ChangesArgs),
}

// Shared output types

#[derive(Serialize)]
pub(super) struct PathRestrictionInfo {
    pub path: String,
    pub roots: Vec<RestrictionRootInfo>,
}

#[derive(Serialize)]
pub(super) struct RestrictionRootInfo {
    pub root_path: String,
    pub acls: Vec<String>,
    pub permission_request_group: Option<String>,
}

// Helper functions

pub(super) fn path_coverage_to_string(coverage: thrift::PathCoverage) -> String {
    match coverage {
        thrift::PathCoverage::NONE => "none".to_string(),
        thrift::PathCoverage::SOME => "some".to_string(),
        thrift::PathCoverage::ALL => "all".to_string(),
        _ => "unknown".to_string(),
    }
}

pub(super) fn convert_restriction_roots(
    roots_map: std::collections::BTreeMap<String, Vec<thrift::PathRestrictionRoot>>,
) -> Vec<PathRestrictionInfo> {
    roots_map
        .into_iter()
        .map(|(path, roots)| PathRestrictionInfo {
            path,
            roots: roots.into_iter().map(convert_restriction_root).collect(),
        })
        .collect()
}

pub(super) fn convert_restriction_root(root: thrift::PathRestrictionRoot) -> RestrictionRootInfo {
    RestrictionRootInfo {
        root_path: root.path,
        acls: root.acls,
        permission_request_group: root.permission_request_group,
    }
}

/// Format the optional permission request group as a trailing display segment,
/// e.g. `, request group: gradient_source_control`. Empty when absent (e.g. an old
/// server that does not populate the field), so output differs only by the segment.
pub(super) fn fmt_request_group(permission_request_group: Option<&str>) -> String {
    match permission_request_group {
        Some(group) => format!(", request group: {group}"),
        None => String::new(),
    }
}

/// Render a single restriction-root line shared by the `access` and `changes`
/// subcommands so both surface the ACLs and request group in an identical format.
pub(super) fn render_restriction_root_line(
    w: &mut dyn Write,
    root: &RestrictionRootInfo,
) -> Result<()> {
    writeln!(
        w,
        "    {} (ACLs: {}{})",
        root.root_path,
        root.acls.join(", "),
        fmt_request_group(root.permission_request_group.as_deref()),
    )?;
    Ok(())
}

pub(super) async fn run(app: ScscApp, args: CommandArgs) -> Result<()> {
    match args.subcommand {
        RestrictedPathsSubcommand::Access(args) => access::run(app, args).await,
        RestrictedPathsSubcommand::Find(args) => find::run(app, args).await,
        RestrictedPathsSubcommand::Changes(args) => changes::run(app, args).await,
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    fn render_line(root: &RestrictionRootInfo) -> String {
        let mut buf = Vec::new();
        render_restriction_root_line(&mut buf, root).expect("render should succeed");
        String::from_utf8(buf).expect("output should be valid utf8")
    }

    /// What it tests: the shared restriction-root line (used by `access` and
    /// `changes`) includes the request group when present and omits the segment when
    /// absent; JSON serialization emits the group string or null.
    /// Expected: Some appends `, request group: g`; None omits it and JSON is null.
    #[mononoke::test]
    fn test_render_restriction_root_line_request_group() {
        let with_group = RestrictionRootInfo {
            root_path: "restricted".to_string(),
            acls: vec!["REPO_REGION:restricted_acl".to_string()],
            permission_request_group: Some("some_group".to_string()),
        };
        assert_eq!(
            render_line(&with_group),
            "    restricted (ACLs: REPO_REGION:restricted_acl, request group: some_group)\n"
        );
        assert_eq!(
            serde_json::to_value(&with_group).expect("json"),
            serde_json::json!({
                "root_path": "restricted",
                "acls": ["REPO_REGION:restricted_acl"],
                "permission_request_group": "some_group",
            }),
        );

        let without_group = RestrictionRootInfo {
            root_path: "restricted".to_string(),
            acls: vec!["REPO_REGION:restricted_acl".to_string()],
            permission_request_group: None,
        };
        assert_eq!(
            render_line(&without_group),
            "    restricted (ACLs: REPO_REGION:restricted_acl)\n"
        );
        assert_eq!(
            serde_json::to_value(&without_group).expect("json"),
            serde_json::json!({
                "root_path": "restricted",
                "acls": ["REPO_REGION:restricted_acl"],
                "permission_request_group": null,
            }),
        );
    }
}
