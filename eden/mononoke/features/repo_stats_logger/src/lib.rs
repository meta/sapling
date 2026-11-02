/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use anyhow::Result;
use blobstore::Loadable;
use bookmarks::ArcBookmarks;
use bookmarks::BookmarkKey;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use futures::future::AbortHandle;
use futures::future::abortable;
use metaconfig_types::ArcRepoConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use repo_blobstore::ArcRepoBlobstore;
use repo_derived_data::ArcRepoDerivedData;
use sharding_ext::encode_repo_name;
use stats::define_stats;
use stats::prelude::DynamicSingletonCounter;
use tracing::warn;

define_stats! {
    prefix = "mononoke.app.repo.stats";
    repo_objects_count: dynamic_singleton_counter("{}.objects.count", (repo_name: String)),
}

#[derive(Clone)]
#[facet::facet]
pub struct RepoStatsLogger {
    abort_handle: AbortHandle,
}

impl RepoStatsLogger {
    pub async fn new(
        fb: FacebookInit,
        repo_name: String,
        repo_config: ArcRepoConfig,
        bookmarks: ArcBookmarks,
        repo_blobstore: ArcRepoBlobstore,
        repo_derived_data: ArcRepoDerivedData,
    ) -> Result<Self, Error> {
        let ctx = CoreContext::new_for_bulk_processing(fb);

        let fut = async move {
            loop {
                let interval = Duration::from_mins(1);
                tokio::time::sleep(interval).await;

                let repo_key = encode_repo_name(&repo_name);
                let bookmark_name =
                    get_repo_bookmark_name(repo_config.clone()).expect("invalid bookmark name");

                match get_repo_objects_count(
                    &ctx,
                    &repo_name,
                    &repo_config,
                    &bookmark_name,
                    bookmarks.clone(),
                    repo_blobstore.clone(),
                    repo_derived_data.clone(),
                )
                .await
                {
                    Ok(count) => {
                        STATS::repo_objects_count.set_value(fb, count, (repo_key,));
                    }
                    Err(e) => {
                        warn!("Finding bookmark for {}: {}", repo_name, e);
                    }
                }
            }
        };

        let (fut, abort_handle) = abortable(fut);
        mononoke::spawn_task(fut);

        Ok(Self { abort_handle })
    }

    // A null implementation that does nothing. Useful for tests.
    pub fn noop() -> Self {
        Self {
            abort_handle: AbortHandle::new_pair().0,
        }
    }
}

fn get_repo_bookmark_name(
    repo_config: Arc<metaconfig_types::RepoConfig>,
) -> Result<BookmarkKey, Error> {
    let bookmark_name = repo_config
        .bookmark_name_for_objects_count
        .clone()
        .unwrap_or("master".to_string());
    BookmarkKey::new(bookmark_name)
}

//
// Returns the settings for repo objectos count computations.
//
// Some of the settings can be specified in both the repo config and a JK. The order of
// precedence is:
//
// * default repo objects count: the JK offers a global default; per-repo configs take precedences.
//   If neither is provided we will fall back to a constant.
// * override objects count: the per-repo config is applied first, but the JK (if available) takes
//   precedence.
// * objects count multiplier: only available per-repo. A JK would not make sense, since the override
//   can be used if waiting for a config change is not desirable.
//
// This is intentional: the default object count is rarely used (only if a repo has no bookmark) and is
// intended as a simple fallback; whereas the override can potentially be used in near-emergency e.g.
// if a repo is causing excessive load.
//
fn get_repo_objects_count_settings(
    repo_name: &str,
    repo_config: Arc<metaconfig_types::RepoConfig>,
) -> (i64, Option<i64>, f32) {
    let default_objects_count = repo_config
        .default_objects_count
        .clone()
        .unwrap_or_else(|| justknobs::get("scm/mononoke:scs_default_repo_objects_count", None));

    // setting the override to 0 means no override
    let maybe_override_objects_count = Some(justknobs::get_as::<i64>(
        "scm/mononoke:scs_override_repo_objects_count",
        Some(repo_name),
    ))
    .filter(|x| *x > 0)
    .or_else(|| repo_config.override_objects_count.clone())
    .filter(|x| *x > 0);

    let objects_count_multiplier = repo_config
        .objects_count_multiplier
        .clone()
        .map_or(1.0, |x| x.deref().clone());

    (
        default_objects_count,
        maybe_override_objects_count,
        objects_count_multiplier,
    )
}

async fn get_repo_objects_count(
    ctx: &CoreContext,
    repo_name: &str,
    repo_config: &ArcRepoConfig,
    bookmark_name: &BookmarkKey,
    bookmarks: ArcBookmarks,
    repo_blobstore: ArcRepoBlobstore,
    repo_derived_data: ArcRepoDerivedData,
) -> Result<i64, Error> {
    let (default_repo_objects_count, maybe_override_objects_count, objects_count_multiplier) =
        get_repo_objects_count_settings(repo_name, repo_config.clone());

    match maybe_override_objects_count {
        Some(over) => {
            // whether the override comes from config or JK, we will skip any computation
            Ok(over)
        }
        None => {
            let mut maybe_bookmark = bookmarks
                .get(
                    ctx.clone(),
                    bookmark_name,
                    // Staleness is rarely close to 1s, so repo_stats_logger should
                    // be able to read bookmark values from replicas
                    bookmarks::Freshness::MaybeStale,
                )
                .await?;
            if repo_config.bookmark_name_for_objects_count.is_none() {
                for fallback_name in ["heads/master", "heads/main"] {
                    if maybe_bookmark.is_some() {
                        break;
                    }
                    maybe_bookmark = bookmarks
                        .get(
                            ctx.clone(),
                            &BookmarkKey::new(fallback_name)?,
                            bookmarks::Freshness::MaybeStale,
                        )
                        .await?;
                }
            }
            if let Some(cs_id) = maybe_bookmark {
                let count = get_descendant_count(
                    ctx,
                    repo_blobstore.clone(),
                    repo_derived_data.clone(),
                    cs_id,
                )
                .await;
                count.map(|count| {
                    ((count as f64) * (objects_count_multiplier as f64)).trunc() as i64
                })
            } else {
                Ok(default_repo_objects_count)
            }
        }
    }
}

async fn get_descendant_count(
    ctx: &CoreContext,
    repo_blobstore: ArcRepoBlobstore,
    repo_derived_data: ArcRepoDerivedData,
    cs_id: ChangesetId,
) -> Result<i64, Error> {
    let root_content_manifest_id = repo_derived_data
        .derive::<RootContentManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await?;
    let content_manifest = root_content_manifest_id
        .into_content_manifest_id()
        .load(ctx, &repo_blobstore)
        .await?;
    let count = content_manifest
        .subentries
        .rollup_data()
        .descendant_counts
        .files_count;
    Ok(i64::try_from(count).expect("file count overflows i64"))
}

impl Drop for RepoStatsLogger {
    fn drop(&mut self) {
        self.abort_handle.abort()
    }
}

#[cfg(test)]
mod tests {
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use bookmarks::BookmarksArc;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use futures::future::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use metaconfig_types::ObjectsCountMultiplier;
    use metaconfig_types::RepoConfig;
    use metaconfig_types::RepoConfigArc;
    use mononoke_macros::mononoke;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreArc;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataArc;
    use repo_identity::RepoIdentity;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;

    use super::*;

    #[facet::container]
    #[derive(Clone)]
    struct Repo {
        #[facet]
        repo_config: RepoConfig,

        #[facet]
        bookmarks: dyn Bookmarks,

        #[facet]
        repo_blobstore: RepoBlobstore,

        #[facet]
        derived_data: RepoDerivedData,

        #[facet]
        repo_identity: RepoIdentity,

        #[facet]
        bonsai_hg_mapping: dyn BonsaiHgMapping,

        #[facet]
        commit_graph: CommitGraph,

        #[facet]
        commit_graph_writer: dyn CommitGraphWriter,

        #[facet]
        filestore_config: FilestoreConfig,
    }

    #[mononoke::fbinit_test]
    async fn test_get_repo_objects_count_settings(fb: FacebookInit) -> Result<(), Error> {
        let factory = TestRepoFactory::new(fb)?;
        let repo: Repo = factory.build().await?;

        let (default_objects_count, maybe_override_objects_count, objects_count_multiplier) =
            get_repo_objects_count_settings("repo", repo.repo_config_arc());
        assert_eq!(default_objects_count, 1000000);
        assert!(maybe_override_objects_count.is_none());
        assert_eq!(objects_count_multiplier, 1.0);

        // set via JK
        let (default_objects_count, maybe_override_objects_count, objects_count_multiplier) =
            with_just_knobs(
                JustKnobsInMemory::new(hashmap![
                    "scm/mononoke:scs_default_repo_objects_count".to_string() => KnobVal::Int(10),
                    "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(20),
                ]),
                || get_repo_objects_count_settings("repo", repo.repo_config_arc()),
            );
        assert_eq!(default_objects_count, 10);
        assert_eq!(maybe_override_objects_count.unwrap(), 20);
        assert_eq!(objects_count_multiplier, 1.0);

        // override JK set to 0 means no override
        let (_default_objects_count, maybe_override_objects_count, _objects_count_multiplier) =
            with_just_knobs(
                JustKnobsInMemory::new(hashmap![
                    "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(0),
                ]),
                || get_repo_objects_count_settings("repo", repo.repo_config_arc()),
            );
        assert!(maybe_override_objects_count.is_none());

        // set in the repo config
        let repo_config = Arc::new(RepoConfig {
            default_objects_count: Some(100),
            override_objects_count: Some(200),
            objects_count_multiplier: Some(ObjectsCountMultiplier::new(3.0)),
            ..Default::default()
        });
        let (default_objects_count, maybe_override_objects_count, objects_count_multiplier) =
            get_repo_objects_count_settings("repo", repo_config);
        assert_eq!(default_objects_count, 100);
        assert_eq!(maybe_override_objects_count.unwrap(), 200);
        assert_eq!(objects_count_multiplier, 3.0);

        // set in both the repo config and in the JK
        let repo_config = Arc::new(RepoConfig {
            default_objects_count: Some(1000),
            override_objects_count: Some(4000),
            objects_count_multiplier: Some(ObjectsCountMultiplier::new(0.005)),
            ..Default::default()
        });
        let (default_objects_count, maybe_override_objects_count, objects_count_multiplier) =
            with_just_knobs(
                JustKnobsInMemory::new(hashmap![
                    "scm/mononoke:scs_default_repo_objects_count".to_string() => KnobVal::Int(2000),
                    "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(3000),
                ]),
                || get_repo_objects_count_settings("repo", repo_config),
            );
        assert_eq!(default_objects_count, 1000);
        assert_eq!(maybe_override_objects_count.unwrap(), 3000);
        assert_eq!(objects_count_multiplier, 0.005);

        // override JK set to 0 means no override; but even so, an explicit override should be honored
        let repo_config = Arc::new(RepoConfig {
            override_objects_count: Some(1000),
            ..Default::default()
        });
        let (_default_objects_count, maybe_override_objects_count, _objects_count_multiplier) =
            with_just_knobs(
                JustKnobsInMemory::new(hashmap![
                    "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(0),
                ]),
                || get_repo_objects_count_settings("repo", repo_config),
            );
        assert_eq!(maybe_override_objects_count.unwrap(), 1000);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_repo_objects_count_empty_repo(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let factory = TestRepoFactory::new(fb)?;
        let repo: Repo = factory.build().await?;
        let bookmark = BookmarkKey::new("master")?;

        let repo_config = repo.repo_config_arc();
        let bookmarks = repo.bookmarks_arc();
        let repo_blobstore = repo.repo_blobstore_arc();
        let repo_derived_data = repo.repo_derived_data_arc();

        // set a default via JK
        let get_count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        );
        let count = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:scs_default_repo_objects_count".to_string() => KnobVal::Int(42)
            ]),
            get_count.boxed(),
        )
        .await?;
        assert_eq!(count, 42);

        // override via JK
        let get_count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        );
        let count = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:scs_default_repo_objects_count".to_string() => KnobVal::Int(42),
                "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(15),
            ]),
            get_count.boxed(),
        )
        .await?;
        assert_eq!(count, 15);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_repo_objects_count(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let factory = TestRepoFactory::new(fb)?;
        let repo: Repo = factory.build().await?;

        let first = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("foo/bar/a", "a")
            .add_file("foo/bar/b/d", "d")
            .add_file("foo/bar/b/e", "e")
            .add_file("foo/bar/c/f", "f")
            .add_file("foo/bar/c/g", "g")
            .commit()
            .await?;
        let bookmark = bookmark(&ctx, &repo, "master").set_to(first).await?;

        let repo_config = repo.repo_config_arc();
        let bookmarks = repo.bookmarks_arc();
        let repo_blobstore = repo.repo_blobstore_arc();
        let repo_derived_data = repo.repo_derived_data_arc();

        let get_count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        );

        // actual count
        let count = get_count.await?;
        assert_eq!(count, 5);

        // set a default via JK
        let get_count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        );
        let count = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:scs_default_repo_objects_count".to_string() => KnobVal::Int(42)
            ]),
            get_count.boxed(),
        )
        .await?;
        assert_eq!(count, 5);

        // override via JK
        let get_count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        );
        let count = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:scs_default_repo_objects_count".to_string() => KnobVal::Int(42),
                "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(15),
            ]),
            get_count.boxed(),
        )
        .await?;
        assert_eq!(count, 15);

        // an override set to 0 should be ignored
        let get_count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        );
        let count = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:scs_override_repo_objects_count".to_string() => KnobVal::Int(0),
            ]),
            get_count.boxed(),
        )
        .await?;
        assert_eq!(count, 5);

        // set a multiplier
        let repo_config = Arc::new(RepoConfig {
            default_objects_count: Some(100),
            objects_count_multiplier: Some(ObjectsCountMultiplier::new(0.5)),
            ..Default::default()
        });
        let count = get_repo_objects_count(
            &ctx,
            "repo",
            &repo_config,
            &bookmark,
            bookmarks.clone(),
            repo_blobstore.clone(),
            repo_derived_data.clone(),
        )
        .await?;
        assert_eq!(count, 2);

        Ok(())
    }
}
