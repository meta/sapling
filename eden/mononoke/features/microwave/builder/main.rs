/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod microwave_filenodes;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Error;
use anyhow::format_err;
use blobrepo_override::DangerousOverride;
use blobstore_factory::BlobstoreArgDefaults;
use blobstore_factory::PutBehaviour;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Bookmarks;
use bookmarks::BookmarksRef;
use bookmarks_cache::WarmerRequirement;
use cache_warmup::CacheWarmupKind;
use cache_warmup::CacheWarmupRequest;
use cache_warmup::CacheWarmupTarget;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use cloned::cloned;
use commit_graph::CommitGraph;
use context::CoreContext;
use context::SessionContainer;
use fbinit::FacebookInit;
use filenodes::ArcFilenodes;
use filenodes::Filenodes;
use filenodes_derivation::FilenodesOnlyPublic;
use futures::channel::mpsc;
use futures::future;
use mercurial_derivation::MappedHgChangesetId;
use metaconfig_types::CacheWarmupParams;
use metaconfig_types::RepoConfigSource;
use microwave::Snapshot;
use microwave::SnapshotLocation;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_macros::mononoke;
use mutable_blobstore::MutableRepoBlobstore;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataArc;
use repo_identity::RepoIdentity;
use tracing::Instrument;
use tracing::info;
use warm_bookmarks_cache::LatestDerivedBookmarkEntry;
use warm_bookmarks_cache::create_derived_data_warmer;
use warm_bookmarks_cache::find_latest_derived_and_underived;

use crate::microwave_filenodes::MicrowaveFilenodes;

#[facet::container]
#[derive(Clone)]
struct Repo {
    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    mutable_repo_blobstore: MutableRepoBlobstore,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    bookmark_update_log: dyn BookmarkUpdateLog,

    #[facet]
    filenodes: dyn Filenodes,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,
}

impl DangerousOverride<ArcFilenodes> for Repo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(ArcFilenodes) -> ArcFilenodes,
    {
        let filenodes = modify(self.filenodes.clone());
        let repo_derived_data = Arc::new(
            self.repo_derived_data
                .with_replaced_filenodes(filenodes.clone()),
        );
        Self {
            filenodes,
            repo_derived_data,
            ..self.clone()
        }
    }
}

async fn cache_warmup_target(
    ctx: &CoreContext,
    repo: &Repo,
    bookmark: &BookmarkKey,
) -> Result<CacheWarmupTarget, Error> {
    use warm_bookmarks_cache::WarmerTag;

    let warmers = vec![
        create_derived_data_warmer::<MappedHgChangesetId>(
            ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg],
        ),
        create_derived_data_warmer::<FilenodesOnlyPublic>(
            ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg],
        ),
    ];

    let latest_bookmarks = find_latest_derived_and_underived(
        ctx,
        repo.bookmarks(),
        repo.bookmark_update_log(),
        bookmark,
        &warmers,
    )
    .await?;

    match latest_bookmarks.derived_entries[WarmerRequirement::AllKinds] {
        LatestDerivedBookmarkEntry::Found(Some((cs_id, _))) => {
            Ok(CacheWarmupTarget::Changeset(cs_id))
        }
        LatestDerivedBookmarkEntry::Found(None) => {
            Err(format_err!("Bookmark {bookmark} has no derived data"))
        }
        LatestDerivedBookmarkEntry::NotFound => Err(format_err!(
            "Bookmark {bookmark} has too many underived commits"
        )),
    }
}

async fn async_main(app: MononokeApp) -> Result<(), Error> {
    let env = app.environment();
    let args: MononokeMicrowaveArgs = app.args()?;

    let repo_factory = Arc::clone(app.repo_factory());
    let scuba = env.scuba_sample_builder.clone();

    // Enumerate via `load_all_repo_configs` so split-loaded repos
    // (only present in the per-tier RepoSpec manifest) also get a
    // microwave snapshot — iterating `repo_configs.repos` would
    // silently skip them and their cache warmup would have no
    // microwave preload, increasing cold-start latency.
    let repos = app.configs().load_all_repo_configs()?;

    let location = match &args.command {
        Commands::LocalPath(local_path_args) => {
            let path = &local_path_args.local_path;
            info!("Writing to path {}", path.display());
            SnapshotLocation::SharedLocalPath(path.as_path())
        }
        Commands::Blobstore => SnapshotLocation::Blobstore,
    };
    let common_config = &app.repo_configs().common;
    let futs = repos
        .into_iter()
        .map(|(name, config)| {
            cloned!(repo_factory, mut scuba, common_config);
            let span = tracing::info_span!("microwave builder", repo = %name);
            async move {
                let ctx = {
                    scuba.add("reponame", name.clone());
                    let session = SessionContainer::new_with_defaults(app.fb);
                    session.new_context(scuba)
                };

                let (filenodes_sender, filenodes_receiver) = mpsc::channel(1000);
                let warmup_ctx = ctx.clone();

                let warmup = async move {
                    let cache_warmup = config.cache_warmup.clone();
                    let repo: Repo = repo_factory
                        .build(name, config, common_config, RepoConfigSource::default())
                        .await?;

                    // Rewind bookmarks to the point where we have derived data. Cache
                    // warmup requires filenodes to be present.
                    let req = match cache_warmup {
                        Some(params) => {
                            let CacheWarmupParams {
                                bookmark,
                                commit_limit,
                                microwave_preload,
                            } = params;

                            let target = cache_warmup_target(&warmup_ctx, &repo, &bookmark).await?;

                            Some(CacheWarmupRequest {
                                target,
                                commit_limit,
                                microwave_preload,
                            })
                        }
                        None => None,
                    };

                    let warmup_repo = repo.dangerous_override(|inner| -> ArcFilenodes {
                        Arc::new(MicrowaveFilenodes::new(filenodes_sender, inner))
                    });

                    cache_warmup::cache_warmup(
                        &warmup_ctx,
                        &warmup_repo,
                        req,
                        CacheWarmupKind::MicrowaveBuilder,
                    )
                    .await?;

                    Result::<_, Error>::Ok(repo)
                };

                let handle = mononoke::spawn_task(warmup);
                let snapshot = Snapshot::build(filenodes_receiver).await;

                // Make sure cache warmup has succeeded before committing this snapshot, and get
                // the repo back.
                let repo = handle.await??;

                snapshot.commit(&ctx, &repo, location).await?;

                Result::<_, Error>::Ok(())
            }
            .instrument(span)
        })
        .collect::<Vec<_>>();

    future::try_join_all(futs).await?;

    Ok(())
}

#[derive(Parser)]
#[clap(name = "Mononoke Local Replay")]
struct MononokeMicrowaveArgs {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[clap(name = "local-path", about = "Write cache priming data to path")]
    LocalPath(LocalPath),
    #[clap(
        name = "blobstore",
        about = "Write cache priming data to the repository blobstore"
    )]
    Blobstore,
}

#[derive(Args)]
struct LocalPath {
    #[clap(name = "local-path", value_parser)]
    local_path: PathBuf,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let app = MononokeAppBuilder::new(fb)
        .with_app_extension(MonitoringAppExtension {})
        .with_arg_defaults(BlobstoreArgDefaults {
            put_behaviour: Some(PutBehaviour::Overwrite),
            ..Default::default()
        })
        .build::<MononokeMicrowaveArgs>()?;

    app.run_with_monitoring_and_logging(async_main, "microwave", AliveService)
}
