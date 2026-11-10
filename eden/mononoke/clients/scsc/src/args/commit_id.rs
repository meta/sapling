/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Arguments for Commit IDs and Commit Identity Schemes

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use anyhow::Error;
use anyhow::format_err;
use clap::Args;
use commit_id_types::CommitId;
use faster_hex::hex_string;
use futures::future::FutureExt;
use futures::future::try_join_all;
use futures::stream::FuturesOrdered;
use futures::stream::TryStreamExt;
use scs_client_raw::ScsClient;
use scs_client_raw::thrift;

use crate::errors::SelectionErrorExt;

#[derive(strum::EnumString, strum::Display, Clone, PartialEq, Eq, Hash)]
#[strum(serialize_all = "kebab-case")]
pub(crate) enum Scheme {
    Bonsai,
    Hg,
    Git,
    Globalrev,
    Svnrev,
    P4Changelist,
}

impl Scheme {
    pub(crate) fn into_thrift(self) -> thrift::CommitIdentityScheme {
        match self {
            Self::Hg => thrift::CommitIdentityScheme::HG,
            Self::Bonsai => thrift::CommitIdentityScheme::BONSAI,
            Self::Git => thrift::CommitIdentityScheme::GIT,
            Self::Globalrev => thrift::CommitIdentityScheme::GLOBALREV,
            Self::Svnrev => thrift::CommitIdentityScheme::SVNREV,
            Self::P4Changelist => thrift::CommitIdentityScheme::P4_CHANGELIST,
        }
    }
}

#[derive(Args, Clone)]
pub struct SchemeArgs {
    #[clap(long, short('S'), required = false, value_delimiter = ',')]
    /// Commit identity schemes to display
    schemes: Vec<Scheme>,
}

impl SchemeArgs {
    pub(crate) fn into_request_schemes(self) -> BTreeSet<thrift::CommitIdentityScheme> {
        self.schemes.into_iter().map(Scheme::into_thrift).collect()
    }

    pub(crate) fn scheme_string_set(&self) -> HashSet<String> {
        self.schemes.iter().map(ToString::to_string).collect()
    }
}

/// Try to resolve a bookmark name to a commit ID.
async fn try_resolve_bookmark(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    bookmark: impl Into<String>,
) -> Result<Option<thrift::CommitId>, Error> {
    let params = thrift::RepoResolveBookmarkParams {
        bookmark_name: bookmark.into(),
        identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let response = conn
        .repo_resolve_bookmark(repo, &params)
        .await
        .map_err(|e| e.handle_selection_error(repo))?;
    Ok(response
        .ids
        .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
}

/// Try to resolve a hex string to an hg commit ID (it can be prefix of the full hash)
async fn try_resolve_hg_commit_id(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    value: impl AsRef<str>,
) -> Result<Option<thrift::CommitId>, Error> {
    // Possible prefix should be valid to be passed to `repo_resolve_commit_prefix`:
    if value.as_ref().len() > 40 || value.as_ref().chars().any(|c| !c.is_ascii_hexdigit()) {
        return Ok(None);
    }

    let resp = conn
        .repo_resolve_commit_prefix(
            repo,
            &thrift::RepoResolveCommitPrefixParams {
                identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
                    .into_iter()
                    .collect(),
                prefix_scheme: thrift::CommitIdentityScheme::HG,
                prefix: value.as_ref().into(),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.handle_selection_error(repo))?;

    if let thrift::RepoResolveCommitPrefixResponseType::AMBIGUOUS = resp.resolved_type {
        eprintln!(
            "note: several hg commits with the prefix '{}' exist",
            value.as_ref()
        );
    }

    Ok(resp
        .ids
        .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
}

/// Try to resolve a hex string to a bonsai changeset ID (it can be prefix of the full hash)
async fn try_resolve_bonsai_id(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    value: impl AsRef<str>,
) -> Result<Option<thrift::CommitId>, Error> {
    // Possible prefix should be valid to be passed to `repo_resolve_commit_prefix`:
    if value.as_ref().len() > 64 || value.as_ref().chars().any(|c| !c.is_ascii_hexdigit()) {
        return Ok(None);
    }

    let resp = conn
        .repo_resolve_commit_prefix(
            repo,
            &thrift::RepoResolveCommitPrefixParams {
                identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
                    .into_iter()
                    .collect(),
                prefix_scheme: thrift::CommitIdentityScheme::BONSAI,
                prefix: value.as_ref().into(),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.handle_selection_error(repo))?;

    if let thrift::RepoResolveCommitPrefixResponseType::AMBIGUOUS = resp.resolved_type {
        eprintln!(
            "note: several bonsai commits with the prefix '{}' exist",
            value.as_ref()
        );
    }

    Ok(resp
        .ids
        .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
}

/// Try to resolve a git sha1 string to a commit ID.
async fn try_resolve_git_sha1(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    value: impl AsRef<str>,
) -> Result<Option<thrift::CommitId>, Error> {
    // Possible prefix should be valid to be passed to `repo_resolve_commit_prefix`:
    if value.as_ref().len() > 40 || value.as_ref().chars().any(|c| !c.is_ascii_hexdigit()) {
        return Ok(None);
    }

    let resp = conn
        .repo_resolve_commit_prefix(
            repo,
            &thrift::RepoResolveCommitPrefixParams {
                identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
                    .into_iter()
                    .collect(),
                prefix_scheme: thrift::CommitIdentityScheme::GIT,
                prefix: value.as_ref().into(),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.handle_selection_error(repo))?;

    if let thrift::RepoResolveCommitPrefixResponseType::AMBIGUOUS = resp.resolved_type {
        eprintln!(
            "note: several git commits with the prefix '{}' exist",
            value.as_ref()
        );
    }
    Ok(resp
        .ids
        .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
}

/// Try to resolve a globalrev string to a commit ID.
async fn try_resolve_globalrev(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    globalrev: impl AsRef<str>,
) -> Result<Option<thrift::CommitId>, Error> {
    match globalrev.as_ref().parse::<i64>() {
        Ok(globalrev) => {
            let params = thrift::CommitLookupParams {
                identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let commit = thrift::CommitSpecifier {
                repo: repo.clone(),
                id: thrift::CommitId::globalrev(globalrev),
                ..Default::default()
            };
            let response = conn
                .commit_lookup(&commit, &params)
                .await
                .map_err(|e| e.handle_selection_error(repo))?;
            Ok(response
                .ids
                .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
        }
        Err(_) => Ok(None),
    }
}

/// Try to resolve a svn revision number to a commit ID.
async fn try_resolve_svnrev(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    globalrev: impl AsRef<str>,
) -> Result<Option<thrift::CommitId>, Error> {
    match globalrev.as_ref().parse::<i64>() {
        Ok(globalrev) => {
            let params = thrift::CommitLookupParams {
                identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let commit = thrift::CommitSpecifier {
                repo: repo.clone(),
                id: thrift::CommitId::svnrev(globalrev),
                ..Default::default()
            };
            let response = conn
                .commit_lookup(&commit, &params)
                .await
                .map_err(|e| e.handle_selection_error(repo))?;
            Ok(response
                .ids
                .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
        }
        Err(_) => Ok(None),
    }
}

/// Try to resolve a Perforce changelist number to a commit ID.
async fn try_resolve_p4_changelist(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    changelist: impl AsRef<str>,
) -> Result<Option<thrift::CommitId>, Error> {
    match changelist.as_ref().parse::<i64>() {
        Ok(changelist) => {
            let params = thrift::CommitLookupParams {
                identity_schemes: Some(thrift::CommitIdentityScheme::BONSAI)
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let commit = thrift::CommitSpecifier {
                repo: repo.clone(),
                id: thrift::CommitId::p4_changelist(changelist),
                ..Default::default()
            };
            let response = conn
                .commit_lookup(&commit, &params)
                .await
                .map_err(|e| e.handle_selection_error(repo))?;
            Ok(response
                .ids
                .and_then(|ids| ids.get(&thrift::CommitIdentityScheme::BONSAI).cloned()))
        }
        Err(_) => Ok(None),
    }
}

/// Resolve commit IDs into a thrift commit identifier.
///
/// Simple commit ID types (hashes of a known type) are converted to the right
/// commit ID variant.
///
/// Other commit ID types, like bookmark names or hashes of an unknown type may
/// involve a call to the server to resolve.
pub(crate) async fn resolve_commit_ids(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    commit_ids: impl IntoIterator<Item = &CommitId>,
) -> Result<Vec<thrift::CommitId>, Error> {
    commit_ids
        .into_iter()
        .map(|commit_id| {
            async move {
                match commit_id {
                    CommitId::BonsaiId(bonsai) => Ok(thrift::CommitId::bonsai(bonsai.to_vec())),
                    CommitId::EphemeralBonsai(bonsai) => Ok(thrift::CommitId::ephemeral_bonsai(
                        thrift::EphemeralBonsai {
                            bonsai_id: bonsai.to_vec(),
                            bubble_id: 0,
                            ..Default::default()
                        },
                    )),
                    CommitId::HgId(hg) => Ok(thrift::CommitId::hg(hg.to_vec())),
                    CommitId::GitSha1(hash) => Ok(thrift::CommitId::git(hash.to_vec())),
                    CommitId::Globalrev(rev) => Ok(thrift::CommitId::globalrev((*rev).try_into()?)),
                    CommitId::Svnrev(rev) => Ok(thrift::CommitId::svnrev((*rev).try_into()?)),
                    CommitId::P4Changelist(cl) => {
                        Ok(thrift::CommitId::p4_changelist((*cl).try_into()?))
                    }
                    CommitId::Bookmark(bookmark) => try_resolve_bookmark(conn, repo, bookmark)
                        .await?
                        .ok_or_else(|| format_err!("bookmark not found: {bookmark}")),
                    CommitId::Resolve(commit_id) => {
                        let resolvers = vec![
                            try_resolve_bonsai_id(conn, repo, commit_id).boxed(),
                            try_resolve_hg_commit_id(conn, repo, commit_id).boxed(),
                            try_resolve_git_sha1(conn, repo, commit_id).boxed(),
                            try_resolve_globalrev(conn, repo, commit_id).boxed(),
                            try_resolve_svnrev(conn, repo, commit_id).boxed(),
                            try_resolve_p4_changelist(conn, repo, commit_id).boxed(),
                        ];
                        let candidates: Vec<_> = try_join_all(resolvers)
                            .await?
                            .into_iter()
                            .flatten()
                            .collect();
                        match candidates.as_slice() {
                            [] => Err(format_err!("commit not found: {commit_id}")),
                            [id] => Ok(id.clone()),
                            _ => {
                                // This commit ID resolves to different
                                // commits in different schemes. This is a
                                // cross-scheme hash collision (e.g. two
                                // commits exist where the Hg commit ID of
                                // one matches the Git commit ID of the
                                // other).
                                //
                                // In practice this should be very rare, but
                                // handle it here. Users will need to
                                // specify which scheme should be used by
                                // using the appropriate argument to specify
                                // the ID (e.g. --hg-commit-id).
                                Err(format_err!("ambiguous commit id: {commit_id}"))
                            }
                        }
                    }
                }
            }
        })
        .collect::<FuturesOrdered<_>>()
        .try_collect()
        .await
}

/// Resolve a single commit ID.
pub(crate) async fn resolve_commit_id(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    commit_id: &CommitId,
) -> Result<thrift::CommitId, Error> {
    let commit_ids = resolve_commit_ids(conn, repo, Some(commit_id)).await?;
    Ok(commit_ids.into_iter().next().expect("commit id expected"))
}

pub(crate) async fn resolve_optional_commit_id(
    conn: &ScsClient,
    repo: &thrift::RepoSpecifier,
    commit_id: Option<&CommitId>,
) -> Result<Option<thrift::CommitId>, Error> {
    if let Some(commit_id) = commit_id {
        Ok(Some(resolve_commit_id(conn, repo, commit_id).await?))
    } else {
        Ok(None)
    }
}

/// Map commit IDs to the scheme name and string representation of the commit ID.
pub(crate) fn map_commit_ids<'a>(
    ids: impl Iterator<Item = &'a thrift::CommitId>,
) -> BTreeMap<String, String> {
    ids.filter_map(map_commit_id).collect()
}

/// Map a commit ID to its scheme name and string representation.
pub(crate) fn map_commit_id(id: &thrift::CommitId) -> Option<(String, String)> {
    match id {
        thrift::CommitId::bonsai(hash) => Some((String::from("bonsai"), hex_string(hash))),
        thrift::CommitId::hg(hash) => Some((String::from("hg"), hex_string(hash))),
        thrift::CommitId::git(hash) => Some((String::from("git"), hex_string(hash))),
        thrift::CommitId::globalrev(rev) => Some((String::from("globalrev"), rev.to_string())),
        thrift::CommitId::svnrev(rev) => Some((String::from("svnrev"), rev.to_string())),
        thrift::CommitId::p4_changelist(cl) => {
            Some((String::from("p4_changelist"), cl.to_string()))
        }
        _ => None,
    }
}
