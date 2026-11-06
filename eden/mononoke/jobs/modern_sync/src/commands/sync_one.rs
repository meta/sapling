/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use anyhow::format_err;
use clap::Parser;
use futures::channel::oneshot;
use mononoke_app::MononokeApp;
use mononoke_types::ChangesetId;
use mutable_counters::MutableCountersArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use url::Url;

use crate::ModernSyncArgs;
use crate::Repo;
use crate::sender::edenapi::DefaultEdenapiSenderBuilder;
use crate::sender::edenapi::EdenapiConfig;
use crate::sender::edenapi::EdenapiSender;
use crate::sender::manager::ChangesetMessage;
use crate::sender::manager::SendManager;

/// Sync one changeset (debug only)
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(long, help = "Changeset to sync")]
    cs_id: ChangesetId,

    #[clap(flatten, next_help_heading = "SYNC OPTIONS")]
    sync_args: crate::sync::SyncArgs,
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let app_args = &app.args::<ModernSyncArgs>()?;
    let args = Arc::new(args);
    let sync_args = &args.clone().sync_args;
    let repo: Repo = app.open_repo(&sync_args.repo).await?;
    let _repo_id = repo.repo_identity().id();
    let repo_name = repo.repo_identity().name().to_string();

    let config = repo
        .repo_config
        .modern_sync_config
        .clone()
        .ok_or(format_err!(
            "No modern sync config found for repo {repo_name}"
        ))?;

    let ctx = crate::sync::build_context(Arc::new(app), &repo_name, false);

    let sender: Arc<dyn EdenapiSender + Send + Sync> = {
        let url = if let Some(socket) = app_args.edenapi_args.dest_socket {
            // Only for integration tests
            format!("{}:{}/edenapi/", config.url, socket)
        } else {
            format!("{}/edenapi/", config.url)
        };

        let tls_args = app_args
            .edenapi_args
            .tls_params
            .clone()
            .ok_or_else(|| format_err!("TLS params not found for repo {repo_name}"))?;

        let dest_repo = sync_args
            .dest_repo_name
            .clone()
            .unwrap_or(repo_name.clone());

        let edenapi_config = EdenapiConfig {
            url: Url::parse(&url)?,
            tls_args,
            http_proxy_host: app_args.edenapi_args.http_proxy_host.clone(),
            http_no_proxy: app_args.edenapi_args.http_no_proxy.clone(),
        };

        Arc::new(
            DefaultEdenapiSenderBuilder::new(ctx.clone(), edenapi_config, dest_repo, &repo)
                .build()
                .await?,
        )
    };

    let cancellation_requested = Arc::new(AtomicBool::new(false));
    let mut send_manager = SendManager::new(
        ctx.clone(),
        &config,
        repo.repo_blobstore().clone(),
        sender.clone(),
        repo_name.clone(),
        PathBuf::from(""),
        repo.mutable_counters_arc(),
        cancellation_requested,
    );

    let messages = crate::sync::process_one_changeset(&args.cs_id, &ctx, repo, false, "").await;
    crate::sync::send_messages_in_order(messages, &mut send_manager).await?;
    let (finish_tx, finish_rx) = oneshot::channel();
    send_manager
        .send_changeset(ChangesetMessage::NotifyCompletion(finish_tx))
        .await?;
    finish_rx.await??;

    Ok(())
}
