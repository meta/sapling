/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use bonsai_git_mapping::BonsaiGitMappingRef;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingRef;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bonsai_p4_mapping::BonsaiP4MappingRef;
use bonsai_svnrev_mapping::BonsaiSvnrevMappingRef;
use bookmarks::BookmarkKey;
use bookmarks::BookmarksRef;
use clap::ValueEnum;
pub use commit_id_types::*;
use context::CoreContext;
use futures::TryStreamExt;
use futures::future::join;
use futures::stream::FuturesOrdered;
use mercurial_types::HgChangesetId;
use mononoke_types::ChangesetId;
use mononoke_types::Globalrev;
use mononoke_types::P4ChangelistId;
use mononoke_types::Svnrev;
use mononoke_types::hash::GitSha1;
use strum::Display;

pub trait Repo = BonsaiHgMappingRef
    + BonsaiGitMappingRef
    + BonsaiGlobalrevMappingRef
    + BonsaiSvnrevMappingRef
    + BonsaiP4MappingRef;

#[derive(Copy, Clone, Eq, PartialEq, ValueEnum, Display)]
#[strum(serialize_all = "kebab_case")]
pub enum IdentityScheme {
    /// Mononoke bonsai hash
    Bonsai,

    /// Mercurial hash
    Hg,

    /// Git SHA-1 hash
    Git,

    /// Globalrev
    Globalrev,

    /// Subversion revision (legacy)
    Svnrev,

    /// Perforce changelist number (Perforce mirror repos)
    P4Changelist,
}

impl IdentityScheme {
    /// Parse a commit id of this scheme from a string.
    pub async fn parse_commit_id(
        &self,
        ctx: &CoreContext,
        repo: &impl Repo,
        commit_id: &str,
    ) -> Result<ChangesetId> {
        let cs_id = match self {
            IdentityScheme::Bonsai => commit_id
                .parse::<ChangesetId>()
                .context("Invalid bonsai changeset id")?,
            IdentityScheme::Hg => {
                let hg_cs_id = commit_id
                    .parse::<HgChangesetId>()
                    .context("Invalid hg changeset id")?;
                repo.bonsai_hg_mapping()
                    .get_bonsai_from_hg(ctx, hg_cs_id)
                    .await?
                    .ok_or_else(|| anyhow!("hg-bonsai mapping not found for {hg_cs_id}"))?
            }
            IdentityScheme::Git => {
                let git_id = commit_id
                    .parse::<GitSha1>()
                    .context("Invalid git changeset id")?;
                repo.bonsai_git_mapping()
                    .get_bonsai_from_git_sha1(ctx, git_id)
                    .await?
                    .ok_or_else(|| anyhow!("git-bonsai mapping not found for {git_id}"))?
            }
            IdentityScheme::Globalrev => {
                let globalrev = commit_id
                    .parse::<Globalrev>()
                    .context("Invalid globalrev")?;
                repo.bonsai_globalrev_mapping()
                    .get_bonsai_from_globalrev(ctx, globalrev)
                    .await?
                    .ok_or_else(|| anyhow!("globalrev-bonsai mapping not found for {globalrev}"))?
            }
            IdentityScheme::Svnrev => {
                let svnrev = commit_id.parse::<Svnrev>().context("Invalid svnrev")?;
                repo.bonsai_svnrev_mapping()
                    .get_bonsai_from_svnrev(ctx, svnrev)
                    .await?
                    .ok_or_else(|| anyhow!("svnrev-bonsai mapping not found for {svnrev}"))?
            }
            IdentityScheme::P4Changelist => {
                let cl = P4ChangelistId::new(
                    commit_id
                        .parse::<u64>()
                        .context("Invalid p4 changelist number")?,
                );
                repo.bonsai_p4_mapping()
                    .get_bonsai_from_p4_changelist_id(ctx, cl)
                    .await?
                    .ok_or_else(|| {
                        anyhow!("p4-bonsai mapping not found for changelist {}", cl.id())
                    })?
            }
        };
        Ok(cs_id)
    }

    /// Map a commit id into a string for this identity scheme.
    ///
    /// Returns `None` if this commit does not exist in that scheme.
    pub async fn map_commit_id(
        &self,
        ctx: &CoreContext,
        repo: &impl Repo,
        cs_id: ChangesetId,
    ) -> Result<Option<String>> {
        let commit_id = match self {
            IdentityScheme::Bonsai => Some(cs_id.to_string()),
            IdentityScheme::Hg => repo
                .bonsai_hg_mapping()
                .get_hg_from_bonsai(ctx, cs_id)
                .await?
                .as_ref()
                .map(ToString::to_string),
            IdentityScheme::Git => repo
                .bonsai_git_mapping()
                .get_git_sha1_from_bonsai(ctx, cs_id)
                .await?
                .as_ref()
                .map(ToString::to_string),
            IdentityScheme::Globalrev => repo
                .bonsai_globalrev_mapping()
                .get_globalrev_from_bonsai(ctx, cs_id)
                .await?
                .as_ref()
                .map(ToString::to_string),
            IdentityScheme::Svnrev => repo
                .bonsai_svnrev_mapping()
                .get_svnrev_from_bonsai(ctx, cs_id)
                .await?
                .as_ref()
                .map(ToString::to_string),
            IdentityScheme::P4Changelist => repo
                .bonsai_p4_mapping()
                .get_p4_changelist_id_from_bonsai(ctx, cs_id)
                .await?
                .map(|cl| cl.id().to_string()),
        };
        Ok(commit_id)
    }
}

/// Parse a general commit ID from a string
///
/// The string can either be of the form `<scheme>=<id>`, or just
/// a bare id, in which case the scheme will be inferred.
///
/// For inferred schemes, globalrevs should be prefixed by 'm', svnrevs by 's'
/// and Perforce changelists by 'p'.
///
/// Hash types are inferred from their length (64 characters for
/// 32-byte bonsai hashes, 40 characters for 20-byte Mercurial or
/// Git hashes).  For Mercurial and Git, whichever one exists is
/// selected.  In the unlikely event that the hash refers to both
/// a Mercurial commit and a Git commit, the Mercurial commit is
/// returned.
pub async fn parse_commit_id(
    ctx: &CoreContext,
    repo: &impl Repo,
    commit_id: &str,
) -> Result<ChangesetId> {
    if let Some((scheme, id)) = commit_id.split_once('=') {
        let scheme = IdentityScheme::from_str(scheme, /* ignore_case */ true)
            .map_err(|e| anyhow!("Failed to parse commit identity scheme '{scheme}': {e}"))?;
        scheme.parse_commit_id(ctx, repo, id).await
    } else if let Some(globalrev) = commit_id.strip_prefix('m') {
        IdentityScheme::Globalrev
            .parse_commit_id(ctx, repo, globalrev)
            .await
    } else if let Some(svnrev) = commit_id.strip_prefix('s') {
        IdentityScheme::Svnrev
            .parse_commit_id(ctx, repo, svnrev)
            .await
    } else if let Some(changelist) = commit_id.strip_prefix('p') {
        IdentityScheme::P4Changelist
            .parse_commit_id(ctx, repo, changelist)
            .await
    } else if commit_id.len() == 64 {
        IdentityScheme::Bonsai
            .parse_commit_id(ctx, repo, commit_id)
            .await
    } else if commit_id.len() == 40 {
        match join(
            IdentityScheme::Hg.parse_commit_id(ctx, repo, commit_id),
            IdentityScheme::Git.parse_commit_id(ctx, repo, commit_id),
        )
        .await
        {
            (Ok(cs_id), _) => Ok(cs_id),
            (Err(_), Ok(cs_id)) => Ok(cs_id),
            (Err(e), Err(_)) => Err(e),
        }
    } else {
        Err(anyhow!("Invalid commit id: {commit_id}"))
    }
}

/// Print a commit id in the selected schemes.
///
/// If a single scheme is requested, just the commit id is printed, and
/// an error is returned if the commit does not exist in that scheme.
///
/// Otherwise, each commit id is prefixed by the name of the scheme, and
/// schemes for which the commit does not exist are omitted.
///
/// If no schemes are selected, prints the bonsai hash.
pub async fn print_commit_id(
    ctx: &CoreContext,
    repo: &impl Repo,
    schemes: &[IdentityScheme],
    cs_id: ChangesetId,
) -> Result<()> {
    match schemes {
        [] => {
            println!("{cs_id}");
        }
        [scheme] => {
            let commit_id = scheme
                .map_commit_id(ctx, repo, cs_id)
                .await?
                .ok_or_else(|| anyhow!("bonsai-{scheme} mapping not found for {cs_id}"))?;
            println!("{commit_id}");
        }
        schemes => {
            for scheme in schemes {
                if let Some(commit_id) = scheme.map_commit_id(ctx, repo, cs_id).await? {
                    println!("{scheme}: {commit_id}");
                }
            }
        }
    }
    Ok(())
}

/// Resolve commit hashes or bookmarks into ChangesetIds.
pub async fn resolve_commit_ids(
    ctx: &CoreContext,
    repo: &(impl Repo + BookmarksRef),
    commit_ids: impl IntoIterator<Item = &CommitId>,
) -> Result<Vec<ChangesetId>, Error> {
    commit_ids
        .into_iter()
        .map(|commit_id| async move {
            match commit_id {
                CommitId::BonsaiId(bonsai) => ChangesetId::from_bytes(bonsai),
                CommitId::HgId(hg) => {
                    let hg_cs_id = HgChangesetId::from_bytes(hg)?;
                    repo.bonsai_hg_mapping()
                        .get_bonsai_from_hg(ctx, hg_cs_id)
                        .await?
                        .ok_or_else(|| anyhow!("hg-bonsai mapping not found for {hg_cs_id}"))
                }
                CommitId::GitSha1(hash) => {
                    let git_id = GitSha1::from_bytes(hash)?;
                    repo.bonsai_git_mapping()
                        .get_bonsai_from_git_sha1(ctx, git_id)
                        .await?
                        .ok_or_else(|| anyhow!("git-bonsai mapping not found for {git_id}"))
                }
                CommitId::Globalrev(rev) => {
                    let globalrev = Globalrev::new(*rev);
                    repo.bonsai_globalrev_mapping()
                        .get_bonsai_from_globalrev(ctx, globalrev)
                        .await?
                        .ok_or_else(|| {
                            anyhow!("globalrev-bonsai mapping not found for {globalrev}")
                        })
                }
                CommitId::Svnrev(rev) => {
                    let svnrev = Svnrev::new(*rev);
                    repo.bonsai_svnrev_mapping()
                        .get_bonsai_from_svnrev(ctx, svnrev)
                        .await?
                        .ok_or_else(|| anyhow!("svnrev-bonsai mapping not found for {svnrev}"))
                }
                CommitId::P4Changelist(cl) => {
                    let cl = P4ChangelistId::new(*cl);
                    repo.bonsai_p4_mapping()
                        .get_bonsai_from_p4_changelist_id(ctx, cl)
                        .await?
                        .ok_or_else(|| {
                            anyhow!("p4-bonsai mapping not found for changelist {}", cl.id())
                        })
                }
                CommitId::Bookmark(bookmark) => repo
                    .bookmarks()
                    .get(
                        ctx.clone(),
                        &BookmarkKey::new(bookmark)?,
                        bookmarks::Freshness::MostRecent,
                    )
                    .await
                    .and_then(|cs_id| {
                        cs_id.ok_or_else(|| anyhow!("bookmark {bookmark} not found"))
                    }),
                CommitId::Resolve(cs_id_hash) => parse_commit_id(ctx, repo, cs_id_hash).await,
                _ => Err(anyhow!("Unsupported commit id type"))?,
            }
        })
        .collect::<FuturesOrdered<_>>()
        .try_collect()
        .await
}

/// Resolve a single commit ID.
pub async fn resolve_commit_id(
    ctx: &CoreContext,
    repo: &(impl Repo + BookmarksRef),
    commit_id: &CommitId,
) -> Result<ChangesetId> {
    let commit_ids = resolve_commit_ids(ctx, repo, Some(commit_id)).await?;
    Ok(commit_ids.into_iter().next().expect("commit id expected"))
}

pub async fn resolve_optional_commit_id(
    ctx: &CoreContext,
    repo: &(impl Repo + BookmarksRef),
    commit_id: Option<&CommitId>,
) -> Result<Option<ChangesetId>> {
    if let Some(commit_id) = commit_id {
        Ok(Some(resolve_commit_id(ctx, repo, commit_id).await?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use bonsai_git_mapping::BonsaiGitMapping;
    use bonsai_git_mapping::BonsaiGitMappingEntry;
    use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
    use bonsai_globalrev_mapping::BonsaiGlobalrevMappingEntry;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bonsai_hg_mapping::BonsaiHgMappingEntry;
    use bonsai_p4_mapping::BonsaiP4Mapping;
    use bonsai_p4_mapping::BonsaiP4MappingEntry;
    use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
    use bonsai_svnrev_mapping::BonsaiSvnrevMappingEntry;
    use bookmarks::Bookmarks;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[facet::container]
    struct TestRepo {
        #[facet]
        bonsai_hg_mapping: dyn BonsaiHgMapping,

        #[facet]
        bonsai_git_mapping: dyn BonsaiGitMapping,

        #[facet]
        bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

        #[facet]
        bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

        #[facet]
        bonsai_p4_mapping: dyn BonsaiP4Mapping,

        #[facet]
        bookmarks: dyn Bookmarks,
    }

    /// `--bonsai-id`, `--hg-commit-id` and `--git` parse to `CommitId::BonsaiId`, `HgId` and
    /// `GitSha1` holding the raw id bytes. A bonsai id needs no lookup (the changeset id *is* those
    /// bytes); hg and git ids resolve through their mappings, and an unmapped id reports the
    /// missing mapping, as `parse_commit_id` does for the textual forms.
    #[mononoke::fbinit_test]
    async fn resolve_typed_commit_ids_from_their_bytes(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let bcs_id = ChangesetId::from_bytes([0x5a; 32])?;
        let hg_cs_id = HgChangesetId::from_bytes(&[0x3c; 20])?;
        let git_sha1 = GitSha1::from_bytes([0x7e; 20])?;
        repo.bonsai_hg_mapping()
            .add(&ctx, BonsaiHgMappingEntry { hg_cs_id, bcs_id })
            .await?;
        repo.bonsai_git_mapping()
            .add(&ctx, BonsaiGitMappingEntry { git_sha1, bcs_id })
            .await?;

        assert_eq!(
            resolve_commit_id(&ctx, &repo, &CommitId::BonsaiId([0x5a; 32])).await?,
            bcs_id
        );
        assert_eq!(
            resolve_commit_id(&ctx, &repo, &CommitId::HgId([0x3c; 20])).await?,
            bcs_id
        );
        assert_eq!(
            resolve_commit_id(&ctx, &repo, &CommitId::GitSha1([0x7e; 20])).await?,
            bcs_id
        );

        let unmapped = HgChangesetId::from_bytes(&[0x11; 20])?;
        let err = resolve_commit_id(&ctx, &repo, &CommitId::HgId([0x11; 20]))
            .await
            .expect_err("unmapped hg id");
        assert_eq!(
            err.to_string(),
            format!("hg-bonsai mapping not found for {unmapped}")
        );

        Ok(())
    }

    /// `--globalrev` and `--svnrev` parse to `CommitId::Globalrev` and `Svnrev` holding the
    /// number; both resolve through their mappings and an unmapped number reports the missing
    /// mapping, as `parse_commit_id` does for the textual forms.
    #[mononoke::fbinit_test]
    async fn resolve_globalrev_and_svnrev_commit_ids(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let bcs_id = ChangesetId::from_bytes([0x5a; 32])?;
        repo.bonsai_globalrev_mapping()
            .bulk_import(
                &ctx,
                &[BonsaiGlobalrevMappingEntry::new(
                    bcs_id,
                    Globalrev::new(1000147971),
                )],
            )
            .await?;
        repo.bonsai_svnrev_mapping()
            .bulk_import(
                &ctx,
                &[BonsaiSvnrevMappingEntry::new(bcs_id, Svnrev::new(42))],
            )
            .await?;

        assert_eq!(
            resolve_commit_id(&ctx, &repo, &CommitId::Globalrev(1000147971)).await?,
            bcs_id
        );
        assert_eq!(
            resolve_commit_id(&ctx, &repo, &CommitId::Svnrev(42)).await?,
            bcs_id
        );

        let err = resolve_commit_id(&ctx, &repo, &CommitId::Globalrev(1000147972))
            .await
            .expect_err("unmapped globalrev");
        assert_eq!(
            err.to_string(),
            "globalrev-bonsai mapping not found for 1000147972"
        );
        let err = resolve_commit_id(&ctx, &repo, &CommitId::Svnrev(43))
            .await
            .expect_err("unmapped svnrev");
        assert_eq!(err.to_string(), "svnrev-bonsai mapping not found for 43");

        Ok(())
    }

    /// `--p4-changelist 1515` and the bare `p1515` resolve through the p4 mapping; an unmapped
    /// changelist reports the missing mapping like the other numeric schemes.
    #[mononoke::fbinit_test]
    async fn resolve_p4_changelist_commit_ids(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let bcs_id = ChangesetId::from_bytes([0x5b; 32])?;
        repo.bonsai_p4_mapping()
            .bulk_import(
                &ctx,
                &[BonsaiP4MappingEntry::new(bcs_id, P4ChangelistId::new(1515))],
            )
            .await?;

        assert_eq!(
            resolve_commit_id(&ctx, &repo, &CommitId::P4Changelist(1515)).await?,
            bcs_id
        );
        assert_eq!(parse_commit_id(&ctx, &repo, "p1515").await?, bcs_id);
        assert_eq!(
            parse_commit_id(&ctx, &repo, "p4-changelist=1515").await?,
            bcs_id
        );
        assert_eq!(
            IdentityScheme::P4Changelist
                .map_commit_id(&ctx, &repo, bcs_id)
                .await?,
            Some("1515".to_string())
        );

        let err = resolve_commit_id(&ctx, &repo, &CommitId::P4Changelist(1516))
            .await
            .expect_err("unmapped changelist");
        assert_eq!(
            err.to_string(),
            "p4-bonsai mapping not found for changelist 1516"
        );

        Ok(())
    }
}
