/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

use anyhow::Context;
use anyhow::Error;
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bookmarks::BookmarksRef;
use cloned::cloned;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use context::PerfCounterType;
use derivation_queue_thrift::DerivationPriority;
use filenodes::FilenodeResult;
use filenodes::FilenodesRef;
use filenodes_derivation::FilenodesOnlyPublic;
use futures::future;
use futures::future::TryFutureExt;
use futures::stream::TryStreamExt;
use futures_stats::TimedFutureExt;
use manifest::Entry;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::RepoPath;
use mercurial_types::sharded_augmented_manifest::list_all_hg_entries;
use metaconfig_types::CacheWarmupParams;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mutable_blobstore::MutableRepoBlobstoreRef;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub trait Repo = RepoBlobstoreRef
    + MutableRepoBlobstoreRef
    + RepoDerivedDataRef
    + RepoIdentityRef
    + FilenodesRef
    + CommitGraphRef
    + BonsaiHgMappingRef
    + BookmarksRef
    + Send
    + Sync
    + Clone
    + 'static;

mod errors {
    use bookmarks::BookmarkKey;
    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum CacheWarmupError {
        #[error("Bookmark {0} does not exist")]
        BookmarkNotFound(BookmarkKey),
    }
}

#[derive(Debug)]
pub enum CacheWarmupTarget {
    Bookmark(BookmarkKey),
    Changeset(ChangesetId),
}

#[derive(Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum CacheWarmupKind {
    // Warmup done during server startup, it needs happen fast
    MononokeServer,
}

#[derive(Debug)]
pub struct CacheWarmupRequest {
    pub target: CacheWarmupTarget,
    pub commit_limit: usize,
}

impl From<CacheWarmupParams> for CacheWarmupRequest {
    fn from(other: CacheWarmupParams) -> Self {
        Self {
            target: CacheWarmupTarget::Bookmark(other.bookmark),
            commit_limit: other.commit_limit,
        }
    }
}

// Fetches all the manifest entries and their linknodes. Do not fetching files because
// there can be too many of them.
async fn blobstore_and_filenodes_warmup(
    ctx: &CoreContext,
    repo: &impl Repo,
    bcs_id: ChangesetId,
    hg_cs_id: HgChangesetId,
) -> Result<(), Error> {
    // NOTE: Arbitrary number. Tweak somehow?
    let buffer_size = 100usize;

    // Ensure filenodes are derived for this, and load the changeset.
    let (cs, ()) = future::try_join(
        hg_cs_id
            .load(ctx, repo.repo_blobstore())
            .map_err(Error::from),
        repo.repo_derived_data()
            .derive::<FilenodesOnlyPublic>(ctx, bcs_id, DerivationPriority::LOW)
            .map_err(Error::from)
            .map_ok(|_| ()),
    )
    .await?;

    let entries = list_all_hg_entries(
        ctx,
        repo.repo_blobstore().clone(),
        repo.repo_identity().name(),
        cs.manifestid(),
    );
    let null_linknodes = entries
        .try_filter_map(|(path, entry)| {
            let item = match entry {
                Entry::Leaf(_) => None,
                Entry::Tree(hash) => {
                    let hash = HgFileNodeId::new(hash.into_nodehash());

                    let path = match path.into() {
                        Some(path) => RepoPath::DirectoryPath(path),
                        None => RepoPath::RootPath,
                    };

                    cloned!(ctx, repo);
                    let fut = async move {
                        let filenode_res = repo.get_filenode_opt(ctx, &path, hash).await?;
                        let linknode_res = filenode_res
                            .map(|filenode_opt| filenode_opt.map(|filenode| filenode.linknode));
                        match linknode_res {
                            FilenodeResult::Present(maybe_linknode) => Ok(maybe_linknode),
                            FilenodeResult::Disabled => Ok(None),
                        }
                    };

                    Some(fut)
                }
            };

            future::ready(Ok(item))
        })
        .try_buffer_unordered(buffer_size)
        .try_fold(0u64, {
            let mut i = 0;
            move |mut null_linknodes, linknode| {
                if linknode.is_none() {
                    ctx.perf_counters()
                        .increment_counter(PerfCounterType::NullLinknode);
                    null_linknodes += 1;
                }

                i += 1;
                if i % 10000 == 0 {
                    debug!("manifests warmup: fetched {}th entry", i);
                }

                future::ready(Result::<_, Error>::Ok(null_linknodes))
            }
        })
        .await?;

    if null_linknodes > 0 {
        warn!("{} linknodes are missing!", null_linknodes);
    }
    debug!("finished manifests warmup");

    Ok(())
}

async fn commit_graph_segments_warmup(
    ctx: &CoreContext,
    repo: &impl Repo,
    bcs_id: ChangesetId,
) -> Result<(), Error> {
    info!("about to start warming up commit graph segments cache");

    repo.commit_graph()
        .ancestors_difference_segments(ctx, vec![bcs_id], vec![])
        .await?;

    debug!("finished commit graph segments warmup");

    Ok(())
}

async fn do_cache_warmup(
    ctx: &CoreContext,
    repo: &impl Repo,
    target: CacheWarmupTarget,
    cache_warmup_kind: CacheWarmupKind,
) -> Result<(), Error> {
    let ctx = ctx.clone_and_reset();

    let client_correlator = ctx
        .metadata()
        .client_info()
        .and_then(|ci| ci.request_info.as_ref().map(|cri| cri.correlator.as_str()));

    let read_from_replica = justknobs::eval(
        "scm/mononoke:use_maybe_stale_freshness_for_bookmarks",
        client_correlator,
        Some("cache_warmup::do_cache_warmup"),
    );

    let freshness = if read_from_replica {
        bookmarks::Freshness::MaybeStale
    } else {
        bookmarks::Freshness::MostRecent
    };

    let bcs_id = match target {
        CacheWarmupTarget::Bookmark(bookmark) => repo
            .bookmarks()
            .get(ctx.clone(), &bookmark, freshness)
            .await?
            .ok_or(errors::CacheWarmupError::BookmarkNotFound(bookmark))?,
        CacheWarmupTarget::Changeset(bcs_id) => bcs_id,
    };

    let hg_cs_id = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    let blobstore_warmup = mononoke::spawn_task({
        cloned!(ctx, repo);
        async move {
            if cache_warmup_kind == CacheWarmupKind::MononokeServer {
                Ok(())
            } else {
                blobstore_and_filenodes_warmup(&ctx, &repo, bcs_id, hg_cs_id)
                    .await
                    .context("While warming up blobstore and filenodes")
            }
        }
    });

    let commit_graph_segments_warmup = mononoke::spawn_task({
        cloned!(ctx, repo);
        async move {
            commit_graph_segments_warmup(&ctx, &repo, bcs_id)
                .await
                .context("While warming up commit graph segments")
        }
    });

    let (stats, res) = future::try_join(blobstore_warmup, commit_graph_segments_warmup)
        .timed()
        .await;
    let (blobstore_warmup, commit_graph_segments_warmup) = res?;
    blobstore_warmup?;
    commit_graph_segments_warmup?;

    info!("finished initial warmup");

    let mut scuba = ctx.scuba().clone();
    scuba.add_future_stats(&stats);
    ctx.perf_counters().insert_perf_counters(&mut scuba);
    scuba.log_with_msg("Cache warmup complete", None);
    Ok(())
}

/// Fetch all manifest entries for a bookmark, and fetches up to `commit_warmup_limit`
/// ancestors of the bookmark.
pub async fn cache_warmup<T: Into<CacheWarmupRequest>>(
    ctx: &CoreContext,
    repo: &impl Repo,
    cache_warmup: Option<T>,
    cache_warmup_kind: CacheWarmupKind,
) -> Result<(), Error> {
    if let Some(req) = cache_warmup {
        let req = req.into();

        do_cache_warmup(ctx, repo, req.target, cache_warmup_kind)
            .await
            .with_context(|| format!("while warming up repo {}", repo.repo_identity().id()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filenodes::Filenodes;
    use filestore::FilestoreConfig;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use memblob::Memblob;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mutable_blobstore::MutableRepoBlobstore;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_identity::RepoIdentity;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::hg_manifest_reads::HgManifestReadBlocker;

    use super::*;

    #[facet::container]
    #[derive(Clone)]
    struct TestRepo(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        dyn Filenodes,
        FilestoreConfig,
        MutableRepoBlobstore,
        RepoBlobstore,
        RepoDerivedData,
        RepoIdentity,
    );

    /// Hg changesets are derived without their augmented manifests, so that
    /// deriving one never reads Hg manifests.
    fn knobs(read_from_augmented: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(read_from_augmented),
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        })
    }

    /// A repo whose Hg manifest reads fail once the blocker is armed, and a
    /// commit with an hg changeset and filenodes but no augmented manifest.
    async fn repo_with_blocked_hg_manifests(
        ctx: &CoreContext,
    ) -> Result<
        (
            TestRepo,
            Arc<HgManifestReadBlocker>,
            ChangesetId,
            HgChangesetId,
        ),
        Error,
    > {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let repo: TestRepo = TestRepoFactory::new(ctx.fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        let cs_id = CreateCommitContext::new_root(ctx, &repo)
            .add_file("a", "a")
            .add_file("dir/b", "b")
            .commit()
            .await?;
        // Derivation would also derive the augmented manifest, so store the Hg
        // changeset as an upload does, and derive exactly its filenodes.
        fixtures::store_hg_changesets_as_uploaded(ctx, &repo, vec![cs_id]).await?;
        let hg_cs_id = with_just_knobs_async(
            knobs(false),
            async {
                let hg_cs_id = repo.derive_hg_changeset(ctx, cs_id).await?;
                repo.repo_derived_data()
                    .manager()
                    .derive_exactly_batch::<FilenodesOnlyPublic>(ctx, vec![cs_id], None)
                    .await?;
                anyhow::Ok(hg_cs_id)
            }
            .boxed(),
        )
        .await?;
        Ok((repo, blocker, cs_id, hg_cs_id))
    }

    #[mononoke::fbinit_test]
    async fn test_warmup_reads_from_augmented(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker, cs_id, hg_cs_id) = repo_with_blocked_hg_manifests(&ctx).await?;
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, cs_id, DerivationPriority::LOW)
            .await?;

        // When: Hg manifests can no longer be read.
        blocker.arm();

        // Then: the warmup walks the augmented manifest instead.
        with_just_knobs_async(
            knobs(true),
            blobstore_and_filenodes_warmup(&ctx, &repo, cs_id, hg_cs_id).boxed(),
        )
        .await?;
        assert_eq!(
            blocker.reads(),
            0,
            "no Hg manifest is read with the knob on"
        );

        // And: with the knob off, the Hg manifest is still read.
        let knob_off = with_just_knobs_async(
            knobs(false),
            blobstore_and_filenodes_warmup(&ctx, &repo, cs_id, hg_cs_id).boxed(),
        )
        .await;
        assert!(
            knob_off.is_err(),
            "the knob off reads the blocked Hg manifest"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_warmup_missing_augmented(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker, cs_id, hg_cs_id) = repo_with_blocked_hg_manifests(&ctx).await?;
        blocker.arm();

        let err = with_just_knobs_async(
            knobs(true),
            blobstore_and_filenodes_warmup(&ctx, &repo, cs_id, hg_cs_id).boxed(),
        )
        .await
        .expect_err("a missing augmented manifest is an error");
        assert!(
            format!("{err:#}").contains("hgaugmentedmanifest.sha1."),
            "the error names the missing augmented manifest: {err:#}"
        );
        assert_eq!(
            blocker.reads(),
            0,
            "there is no fallback to the Hg manifest"
        );
        Ok(())
    }
}
