/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::format_err;
use blobstore_factory::MetadataSqlFactory;
use bookmarks::BookmarkKey;
use context::CoreContext;
use cross_repo_sync::CommitSyncData;
use live_commit_sync_config::CfgrLiveCommitSyncConfig;
use megarepolib::common::ChangesetArgs as MegarepoNewChangesetArgs;
use megarepolib::common::ChangesetArgsFactory;
use megarepolib::common::StackPosition;
use metaconfig_types::MetadataDatabaseConfig;
use mononoke_api::Repo;
use mononoke_app::MononokeApp;
use mononoke_app::args::AsRepoArg;
use mononoke_types::DateTime;
use pushredirect::SqlPushRedirectionConfigBuilder;
#[cfg(fbcode_build)]
use sql_ext::facebook::MyAdmin;
use sql_ext::replication::NoReplicaLagMonitor;
use sql_ext::replication::ReplicaLagMonitor;
use sql_ext::replication::WaitForReplicationConfig;
use sql_query_config::SqlQueryConfigArc;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tracing::info;

#[derive(Debug, clap::Args, Clone)]
pub(crate) struct ResultingChangesetArgs {
    #[clap(long, short = 'm')]
    pub commit_message: String,
    #[clap(long, short = 'a')]
    pub commit_author: String,

    #[clap(long = "commit-date-rfc3339")]
    pub datetime: Option<String>,

    #[clap(
        long,
        help = "bookmark to point to resulting commits (no sanity checks, will move existing bookmark, be careful)"
    )]
    pub set_bookmark: Option<String>,

    #[clap(long = "mark-public")]
    pub mark_public: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct LightResultingChangesetArgs {
    #[clap(long, short = 'm')]
    pub commit_message: String,

    #[clap(long, short = 'a')]
    pub commit_author: String,

    #[clap(long = "commit-date-rfc3339")]
    pub datetime: Option<String>,
}

impl TryInto<MegarepoNewChangesetArgs> for ResultingChangesetArgs {
    type Error = Error;

    fn try_into(self) -> Result<MegarepoNewChangesetArgs> {
        let mb_datetime = self
            .datetime
            .as_deref()
            .map_or_else(|| Ok(DateTime::now()), DateTime::from_rfc3339)?;

        let mb_bookmark = self.set_bookmark.map(BookmarkKey::new).transpose()?;
        let res = MegarepoNewChangesetArgs {
            message: self.commit_message,
            author: self.commit_author,
            datetime: mb_datetime,
            bookmark: mb_bookmark,
            mark_public: self.mark_public,
        };
        Ok(res)
    }
}

pub(crate) fn get_delete_commits_cs_args_factory(
    res_cs_args: LightResultingChangesetArgs,
) -> Result<Box<dyn ChangesetArgsFactory>> {
    get_commit_factory(res_cs_args, |s, num| -> String {
        format!("[MEGAREPO DELETE] {s} ({num})")
    })
}

pub(crate) fn get_commit_factory(
    res_cs_args: LightResultingChangesetArgs,
    msg_factory: impl Fn(&String, usize) -> String + Send + Sync + 'static,
) -> Result<Box<dyn ChangesetArgsFactory>> {
    let message = res_cs_args.commit_message;
    let author = res_cs_args.commit_author;
    let datetime = res_cs_args
        .datetime
        .as_deref()
        .map_or_else(|| Ok(DateTime::now()), DateTime::from_rfc3339)?;

    Ok(Box::new(move |num: StackPosition| {
        MegarepoNewChangesetArgs {
            author: author.clone(),
            message: msg_factory(&message, num.0),
            datetime,
            bookmark: None,
            mark_public: false,
        }
    }))
}

pub(crate) async fn get_live_commit_sync_config(
    _ctx: &CoreContext,
    app: &MononokeApp,
    repo_args: &impl AsRepoArg,
) -> Result<Arc<CfgrLiveCommitSyncConfig>> {
    let config_store = app.environment().config_store.clone();
    let repo: Arc<Repo> = app.open_repo_unredacted(repo_args).await?;
    let (_, repo_config) = app.repo_config(repo_args.as_repo_arg())?;
    let sql_factory: MetadataSqlFactory = MetadataSqlFactory::new(
        app.fb,
        repo_config.storage_config.metadata.clone(),
        app.mysql_options().clone(),
        *app.readonly_storage(),
    )
    .await?;
    let builder = sql_factory
        .open::<SqlPushRedirectionConfigBuilder>()
        .await?;
    let push_redirection_config = builder.build(repo.sql_query_config_arc());
    let live_commit_sync_config = Arc::new(CfgrLiveCommitSyncConfig::new(
        &config_store,
        Arc::new(push_redirection_config),
    )?);

    Ok(live_commit_sync_config)
}

pub(crate) async fn process_stream_and_wait_for_replication<R: cross_repo_sync::Repo>(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    mut s: impl Stream<Item = Result<u64>> + std::marker::Unpin,
) -> Result<(), Error> {
    let small_repo = commit_sync_data.get_small_repo();
    let large_repo = commit_sync_data.get_large_repo();
    let small_repo_config = small_repo.repo_config();
    let large_repo_config = large_repo.repo_config();
    let small_storage_config_metadata = &small_repo_config.storage_config.metadata;
    let large_storage_config_metadata = &large_repo_config.storage_config.metadata;
    if small_storage_config_metadata != large_storage_config_metadata {
        return Err(format_err!(
            "{} and {} have different db metadata configs: {:?} vs {:?}",
            small_repo.repo_identity().name(),
            large_repo.repo_identity().name(),
            small_storage_config_metadata,
            large_storage_config_metadata,
        ));
    }

    let db_address = match small_storage_config_metadata {
        MetadataDatabaseConfig::Local(_) | MetadataDatabaseConfig::OssRemote(_) => None,
        MetadataDatabaseConfig::Remote(remote_config) => {
            Some(remote_config.production.db_address.clone())
        }
    };

    let wait_config = WaitForReplicationConfig::default();
    let replica_lag_monitor: Arc<dyn ReplicaLagMonitor> = match db_address {
        None => Arc::new(NoReplicaLagMonitor()),
        Some(address) => {
            #[cfg(fbcode_build)]
            {
                let my_admin = MyAdmin::new(ctx.fb).context("building myadmin client")?;
                Arc::new(my_admin.single_shard_lag_monitor(address))
            }
            #[cfg(not(fbcode_build))]
            {
                let _ = ctx;
                let _address = address;
                let _ = anyhow::Ok(()).context("fix compiler warning in OSS mode")?;
                Arc::new(NoReplicaLagMonitor())
            }
        }
    };

    let mut total = 0;
    let mut batch = 0;
    while let Some(chunk_size) = s.try_next().await? {
        total += chunk_size;

        batch += chunk_size;
        if batch < 100 {
            continue;
        }
        info!("processed {} changesets", total);
        batch %= 100;
        replica_lag_monitor
            .wait_for_replication(&|| wait_config.clone())
            .await?;
    }

    Ok(())
}
