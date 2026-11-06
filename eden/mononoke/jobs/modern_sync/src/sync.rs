/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::format_err;
use assembly_line::TryAssemblyLine;
use backsyncer::advance_bookmark_counter;
use backsyncer::get_bookmark_counter_or_initialize;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogArc;
use bookmarks::BookmarkUpdateLogEntry;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use changeset_info::ChangesetInfo;
use clap::Parser;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use cloned::cloned;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use context::SessionContainer;
use derivation_queue_thrift::DerivationPriority;
use filestore::FetchKey;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::channel::oneshot;
use futures::future;
use futures::stream;
use manifest::Entry;
use mercurial_derivation::derive_hg_changeset::DeriveHgChangeset;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::sharded_augmented_manifest::find_intersection_of_hg_diffs;
use metaconfig_types::ModernSyncConfig;
use metadata::Metadata;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use mononoke_app::args::SourceRepoArgs;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::FileChange;
use mononoke_types::sha1_hash::SHA1_HASH_LENGTH_BYTES;
use mutable_counters::ArcMutableCounters;
use mutable_counters::MutableCounters;
use mutable_counters::MutableCountersArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use stats::define_stats;
use stats::prelude::*;
use tokio::sync::Mutex;
use tracing::Instrument;
use url::Url;

use crate::ModernSyncArgs;
use crate::Repo;
use crate::bul_util;
use crate::sender::edenapi;
use crate::sender::edenapi::DefaultEdenapiSenderBuilder;
use crate::sender::edenapi::EdenapiSender;
use crate::sender::edenapi::RetryEdenapiSender;
use crate::sender::manager::BookmarkInfo;
use crate::sender::manager::ChangesetMessage;
use crate::sender::manager::ContentMessage;
use crate::sender::manager::FileMessage;
use crate::sender::manager::MODERN_SYNC_COUNTER_NAME;
use crate::sender::manager::Messages;
use crate::sender::manager::ModernSyncCounterNames;
use crate::sender::manager::SendManager;
use crate::sender::manager::TreeMessage;
use crate::stat;

const SLEEP_INTERVAL_WHEN_CAUGHT_UP: Duration = Duration::from_secs(5);
const BOOKMARK_POLLING_JUST_KNOB: &str = "scm/mononoke:modern_sync_bookmark_polling";

define_stats! {
    prefix = "mononoke.modern_sync.sync";
    changeset_processed_time_ms:  dynamic_timeseries("{}.changeset.processed.time_ms", (repo: String); Average),
    changeset_processed_count:  dynamic_timeseries("{}.changeset.processed.count", (repo: String); Sum),
}

#[derive(Parser)]
pub struct SyncArgs {
    #[clap(flatten)]
    pub repo: RepoArgs,

    #[clap(long)]
    /// "Dest repo name (in case it's different from source repo name)"
    pub dest_repo_name: Option<String>,
}

#[derive(Clone, Copy)]
pub enum ExecutionType {
    SyncOnce,
    Tail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollingMode {
    Legacy,
    Bookmark,
}

impl PollingMode {
    fn counter_names(self, bookmark: &BookmarkKey) -> Result<ModernSyncCounterNames> {
        match self {
            Self::Legacy => Ok(ModernSyncCounterNames::legacy()),
            Self::Bookmark => ModernSyncCounterNames::for_bookmark(bookmark),
        }
    }
}

struct PollingState {
    mode: PollingMode,
    cursor: BookmarkUpdateLogId,
    counter_names: ModernSyncCounterNames,
}

fn polling_mode(repo_name: &str) -> PollingMode {
    if justknobs::eval(BOOKMARK_POLLING_JUST_KNOB, None, Some(repo_name)) {
        PollingMode::Bookmark
    } else {
        PollingMode::Legacy
    }
}

async fn resolve_start_id(
    ctx: &CoreContext,
    counters: &ArcMutableCounters,
    start_id_arg: Option<u64>,
    bookmark: &BookmarkKey,
    mode: PollingMode,
) -> Result<Option<PollingState>> {
    let counter_names = mode.counter_names(bookmark)?;
    let configured_start_id = start_id_arg.map(BookmarkUpdateLogId);
    let mut cursor = match mode {
        PollingMode::Legacy => match configured_start_id {
            Some(start_id) => start_id,
            None => counters
                .get_counter(ctx, &counter_names.completed_entry)
                .await?
                .map(TryInto::try_into)
                .transpose()?
                .ok_or_else(|| {
                    format_err!(
                        "No start-id or mutable counter {} provided",
                        counter_names.completed_entry
                    )
                })?,
        },
        PollingMode::Bookmark => {
            let initial_counter = match configured_start_id {
                Some(start_id) => start_id,
                None => counters
                    .get_counter(ctx, MODERN_SYNC_COUNTER_NAME)
                    .await?
                    .map(TryInto::try_into)
                    .transpose()?
                    .ok_or_else(|| {
                        format_err!(
                            "No start-id or mutable counter {MODERN_SYNC_COUNTER_NAME} provided"
                        )
                    })?,
            };
            let Some(durable_counter) = get_bookmark_counter_or_initialize(
                ctx,
                counters,
                &counter_names.completed_entry,
                initial_counter,
            )
            .await?
            else {
                return Ok(None);
            };
            configured_start_id.unwrap_or(durable_counter)
        }
    };

    if configured_start_id.is_none() {
        let other_counter_names = match mode {
            PollingMode::Legacy => PollingMode::Bookmark.counter_names(bookmark)?,
            PollingMode::Bookmark => PollingMode::Legacy.counter_names(bookmark)?,
        };
        let other_cursor: Option<BookmarkUpdateLogId> = counters
            .get_counter(ctx, &other_counter_names.completed_entry)
            .await?
            .map(TryInto::try_into)
            .transpose()?;

        if let Some(other_cursor) = other_cursor
            && other_cursor > cursor
        {
            tracing::info!(
                "Advancing {:?} polling cursor {} from {} to {} during mode handoff",
                mode,
                counter_names.completed_entry,
                cursor,
                other_cursor,
            );
            advance_bookmark_counter(
                ctx,
                counters,
                &counter_names.completed_entry,
                &mut cursor,
                other_cursor,
            )
            .await?;
        }
    }

    Ok(Some(PollingState {
        mode,
        cursor,
        counter_names,
    }))
}

async fn read_next_entries(
    ctx: &CoreContext,
    repo: &Repo,
    bookmark: &BookmarkKey,
    state: &PollingState,
    limit: u64,
) -> Result<Vec<BookmarkUpdateLogEntry>> {
    let log = repo.bookmark_update_log_arc();
    let entries = match state.mode {
        PollingMode::Legacy => log.read_next_bookmark_log_entries(
            ctx.clone(),
            state.cursor,
            limit,
            Freshness::MaybeStale,
        ),
        PollingMode::Bookmark => log.read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            bookmark.clone(),
            state.cursor,
            limit,
            Freshness::MaybeStale,
        ),
    };
    entries
        .try_collect()
        .await
        .context("While querying bookmarks_update_log")
}

async fn finish_send_manager(send_manager: &SendManager) -> Result<Result<()>> {
    let (finish_tx, finish_rx) = oneshot::channel();
    send_manager
        .send_changeset(ChangesetMessage::NotifyCompletion(finish_tx))
        .await?;
    Ok(finish_rx.await?)
}

pub async fn sync(
    app: Arc<MononokeApp>,
    start_id_arg: Option<u64>,
    source_repo_arg: SourceRepoArgs,
    dest_repo_name: String,
    exec_type: ExecutionType,
    dry_run: bool,
    exit_file: PathBuf,
    sender_decorator: Option<
        Box<
            dyn FnOnce(Arc<dyn EdenapiSender + Send + Sync>) -> Arc<dyn EdenapiSender + Send + Sync>
                + Send
                + Sync,
        >,
    >,
    mc: Option<Arc<dyn MutableCounters + Send + Sync>>,
    cancellation_requested: Arc<AtomicBool>,
) -> Result<()> {
    let repo: Repo = app.open_repo_unredacted(&source_repo_arg).await?;
    let _repo_id = repo.repo_identity().id();
    let repo_name = repo.repo_identity().name().to_string();

    let span = tracing::info_span!("sync", repo = %repo_name);
    async {
        tracing::info!("Opened {source_repo_arg:?} unredacted");

        let repo_blobstore = repo.repo_blobstore();
        let mc = mc.unwrap_or_else(|| repo.mutable_counters_arc());

        let config = repo
            .repo_config
            .modern_sync_config
            .clone()
            .ok_or(format_err!(
                "No modern sync config found for repo {repo_name}"
            ))?;

        let ctx = build_context(app.clone(), &repo_name, dry_run);

        let app_args = app.args::<ModernSyncArgs>()?;
        let bookmark = BookmarkKey::new(app_args.bookmark.clone())?;
        let mut state = loop {
            if cancellation_requested.load(Ordering::Relaxed) {
                tracing::info!("sync stopping due to cancellation request");
                return Ok(());
            }

            let desired_mode = polling_mode(&repo_name);
            let Some(initial_state) =
                resolve_start_id(&ctx, &mc, start_id_arg, &bookmark, desired_mode).await?
            else {
                // Another replica initialized the counter. Read the value it
                // installed on the next polling iteration.
                tokio::time::sleep(SLEEP_INTERVAL_WHEN_CAUGHT_UP).await;
                continue;
            };
            tracing::info!("Starting sync from {}", initial_state.cursor);
            stat::log_sync_start(&ctx, initial_state.cursor.0);
            break initial_state;
        };

        let sender =
            build_edenfs_client(ctx.clone(), &app_args, &dest_repo_name, &config, &repo).await?;
        let sender = if let Some(sender_decorator) = sender_decorator {
            sender_decorator(sender)
        } else {
            sender
        };

        tracing::info!("Established EdenAPI connection");

        let send_manager = SendManager::new(
            ctx.clone(),
            &config,
            repo_blobstore.clone(),
            sender.clone(),
            repo_name.clone(),
            exit_file,
            mc.clone(),
            cancellation_requested.clone(),
        );
        tracing::info!("Initialized channels");
        let mut last_entry = None;

        loop {
            if cancellation_requested.load(Ordering::Relaxed) {
                tracing::info!("sync stopping due to cancellation request");
                break;
            }

            let desired_mode = polling_mode(&repo_name);
            if state.mode != desired_mode {
                // Keep messages from the old execution model ordered before
                // resolving the new model's durable cursor.
                finish_send_manager(&send_manager).await??;
                last_entry = None;
                let Some(new_state) =
                    resolve_start_id(&ctx, &mc, None, &bookmark, desired_mode).await?
                else {
                    // Another replica initialized the counter. Read the value it
                    // installed on the next polling iteration.
                    tokio::time::sleep(SLEEP_INTERVAL_WHEN_CAUGHT_UP).await;
                    continue;
                };
                tracing::info!("Starting sync from {}", new_state.cursor);
                stat::log_sync_start(&ctx, new_state.cursor.0);
                state = new_state;
            }

            let mode = state.mode;
            let counter_names = state.counter_names.clone();
            let mut entries = read_next_entries(
                &ctx,
                &repo,
                &bookmark,
                &state,
                config.single_db_query_entries_limit as u64,
            )
            .await?;

            if let Some(last) = entries.last() {
                state.cursor = last.id;
            } else {
                send_manager
                    .send_changesets(vec![ChangesetMessage::Log((repo_name.clone(), Some(0)))])
                    .await?;
                if matches!(exec_type, ExecutionType::SyncOnce) {
                    break;
                }
                tokio::time::sleep(SLEEP_INTERVAL_WHEN_CAUGHT_UP).await;
                continue;
            }

            tracing::info!("Read {} entries", entries.len());
            if mode == PollingMode::Legacy {
                // An unrelated-only batch does not mean SyncOnce is caught up:
                // a matching bookmark entry may occur later in the global log.
                // Keep scanning until the unfiltered read above returns empty,
                // matching the legacy stream's behavior.
                entries.retain(|entry| {
                    if entry.bookmark_name.name() == bookmark.name() {
                        true
                    } else {
                        tracing::warn!(
                            "Ignoring entry with id {} from branch {}",
                            entry.id,
                            entry.bookmark_name,
                        );
                        false
                    }
                });
            }
            tracing::info!("{} entries left after filtering", entries.len());

            let grouped_entries = if app_args.flatten_bul && !entries.is_empty() {
                let original_size = entries.len();
                let grouped_entries = bul_util::group_entries(entries);
                tracing::info!(
                    "Grouped {} entries into {} macro-entries",
                    original_size,
                    grouped_entries.len()
                );
                grouped_entries
            } else {
                entries
                    .into_iter()
                    .map(bul_util::GroupedBookmarkUpdateLogEntry::single)
                    .collect()
            };

            for grouped_entry in grouped_entries {
                let entry = &grouped_entry.upload_entry;
                let now = std::time::Instant::now();
                process_bookmark_update_log_entry(
                    &ctx,
                    &config,
                    &repo,
                    entry,
                    &grouped_entry.bookmark_entries,
                    &send_manager,
                    sender.clone(),
                    config.chunk_size as u64,
                    app_args.log_to_ods,
                    last_entry,
                    mc.clone(),
                    counter_names.clone(),
                )
                .await
                .inspect(|_| {
                    stat::log_bookmark_update_entry_done(&ctx, &repo_name, entry, now.elapsed());
                })
                .inspect_err(|e| {
                    stat::log_bookmark_update_entry_error(
                        &ctx,
                        &repo_name,
                        entry,
                        e,
                        now.elapsed(),
                    );
                })?;
                last_entry = entry.to_changeset_id;
            }
        }

        if !cancellation_requested.load(Ordering::Relaxed) {
            // Wait for the manager to handle all queued messages and propagate
            // any asynchronous sender failure it reports.
            finish_send_manager(&send_manager).await??;
        }

        Ok(())
    }
    .instrument(span)
    .await
}

pub fn build_context(app: Arc<MononokeApp>, repo_name: &str, dry_run: bool) -> CoreContext {
    let mut metadata = Metadata::default();
    metadata.add_client_info(ClientInfo::default_with_entry_point(
        ClientEntryPoint::ModernSync,
    ));

    let scuba = stat::new(app.clone(), &metadata, repo_name, dry_run);
    let session_container = SessionContainer::builder(app.fb)
        .metadata(Arc::new(metadata))
        .build();

    session_container.new_context(scuba)
}

pub async fn build_edenfs_client(
    ctx: CoreContext,
    app_args: &ModernSyncArgs,
    repo_name: &str,
    config: &ModernSyncConfig,
    source_repo: &Repo,
) -> Result<Arc<dyn EdenapiSender + Send + Sync>> {
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

    let config = edenapi::EdenapiConfig {
        url: Url::parse(&url)?,
        tls_args,
        http_proxy_host: app_args.edenapi_args.http_proxy_host.clone(),
        http_no_proxy: app_args.edenapi_args.http_no_proxy.clone(),
    };

    Ok(Arc::new(RetryEdenapiSender::new(Arc::new(
        DefaultEdenapiSenderBuilder::new(ctx.clone(), config, repo_name.to_string(), source_repo)
            .build()
            .await?,
    ))))
}

pub async fn process_bookmark_update_log_entry(
    ctx: &CoreContext,
    config: &ModernSyncConfig,
    repo: &Repo,
    entry: &BookmarkUpdateLogEntry,
    bookmark_entries: &[BookmarkUpdateLogEntry],
    send_manager: &SendManager,
    sender: Arc<dyn EdenapiSender + Send + Sync>,
    chunk_size: u64,
    log_to_ods: bool,
    last_entry: Option<ChangesetId>,
    mc: Arc<dyn MutableCounters + Send + Sync>,
    counter_names: ModernSyncCounterNames,
) -> Result<()> {
    let repo_name = repo.repo_identity().name().to_string();

    let to_cs = entry
        .to_changeset_id
        .expect("bookmark update log entry should have a destination");

    // If the entry has a source, use it. Otherwise, get it from the bookmark.
    let from_cs = if let Some(cs_id) = entry.from_changeset_id {
        Some(cs_id)
    } else if let Some(cs_id) = last_entry {
        Some(cs_id)
    } else if let Some(hgid) = sender
        .read_bookmark(entry.bookmark_name.as_str().to_owned())
        .await?
    {
        repo.bonsai_hg_mapping()
            .get_bonsai_from_hg(ctx, hgid)
            .await?
    } else {
        None
    };

    let from_vec: Vec<ChangesetId> = from_cs.into_iter().collect();
    let to_vec: Vec<ChangesetId> = vec![to_cs];
    let bookmark_name = entry.bookmark_name.name().to_string();

    let commit_graph = repo.commit_graph().parents_and_subtree_sources_graph();

    let to_generation = commit_graph.changeset_generation(ctx, to_cs).await?;
    let (approx_count, approx_count_str) = if let Some(from_cs) = from_cs {
        let from_generation = commit_graph.changeset_generation(ctx, from_cs).await?;
        let diff = to_generation.difference_from(from_generation);
        if let Some(diff) = diff {
            (Some(diff as i64), format!("approx {diff} commit(s)"))
        } else {
            // This can happen if the bookmark was moved backwards
            let diff = from_generation.difference_from(to_generation);
            if let Some(diff) = diff {
                let neg_diff = if let Ok(diff) = TryInto::<i64>::try_into(diff) {
                    Some(-diff)
                } else {
                    None
                };
                (neg_diff, format!("moved back by approx {diff} commit(s)"))
            } else {
                (
                    None,
                    format!("generation from {from_generation:?} to {to_generation:?}"),
                )
            }
        }
    } else {
        (
            Some(to_generation.value() as i64),
            format!("to generation {:?}", to_generation.value()),
        )
    };
    tracing::info!(
        "Calculating segments for entry {}, from changeset {:?} to changeset {:?}, {}",
        entry.id,
        from_cs,
        to_cs,
        approx_count_str,
    );
    let (_, ctx) = { stat::log_bookmark_update_entry_start(ctx, entry, approx_count) };

    let (commits, latest_checkpoint) = {
        let now = std::time::Instant::now();

        let (commits, latest_checkpoint) = tokio::try_join!(
            async {
                commit_graph
                    .ancestors_difference_segment_slices(&ctx, to_vec, from_vec, chunk_size)
                    .await
                    .with_context(|| "calculating segments")
            },
            async {
                let checkpointed_entry = mc
                    .get_counter(&ctx, &counter_names.current_entry)
                    .await?
                    .unwrap_or(0);

                if checkpointed_entry == entry.id.0 as i64 {
                    Ok(mc
                        .get_counter(&ctx, &counter_names.batch_checkpoint)
                        .await?
                        .unwrap_or(0))
                } else {
                    Ok(0)
                }
            }
        )?;

        tracing::info!(
            "Done calculating segments for entry {}, from changeset {:?} to changeset {:?}, {} in {}ms",
            entry.id,
            from_cs,
            to_cs,
            approx_count_str,
            now.elapsed().as_millis()
        );
        stat::log_bookmark_update_entry_segments_done(
            &ctx,
            &repo_name,
            latest_checkpoint,
            now.elapsed(),
        );

        (commits, latest_checkpoint)
    };

    tracing::info!(
        "Resuming from latest entry checkpoint {}",
        latest_checkpoint
    );

    let skip_batch = (latest_checkpoint as u64) / chunk_size;
    let mut skip_commits = (latest_checkpoint as u64) % chunk_size;

    tracing::info!("Skipping {} batches from entry {}", skip_batch, entry.id);

    let current_position = Arc::new(Mutex::new(latest_checkpoint as u64));

    commits
        .skip(skip_batch as usize)
        .try_for_each(|chunk| {
            cloned!(
                ctx,
                repo,
                sender,
                mut send_manager,
                bookmark_name,
                current_position,
                counter_names
            );
            if skip_commits > 0 {
                tracing::info!("Skipping {} commits within batch", skip_commits);
            }
            let skip = std::mem::replace(&mut skip_commits, 0);

            async move {
                let hgids = stream::iter(chunk)
                    .skip(skip as usize)
                    .map(|cs_id| {
                        cloned!(repo, ctx);
                        async move {
                            let hgid = repo.derive_hg_changeset(&ctx, cs_id).await;
                            (hgid, cs_id)
                        }
                    })
                    .buffered(100)
                    .collect::<Vec<(Result<HgChangesetId, anyhow::Error>, ChangesetId)>>()
                    .await;
                let hgids_len = hgids.len();

                let ids = hgids
                    .into_iter()
                    .map(|(hgid, csid)| Ok((hgid?, csid)))
                    .collect::<Result<Vec<(HgChangesetId, ChangesetId)>>>()?;

                let missing_changesets = sender.filter_existing_commits(ids).await?;
                let ms_len = missing_changesets.len();
                let existing_changesets = hgids_len - ms_len;
                *current_position.lock().await += existing_changesets as u64;

                let entry_id = entry.id.0 as i64;
                tracing::info!(
                    "Starting sync of {} missing commits, {} were already synced",
                    missing_changesets.len(),
                    existing_changesets
                );

                stream::iter(missing_changesets)
                    .map(|cs_id| {
                        cloned!(ctx, repo, bookmark_name);

                        mononoke::spawn_task(async move {
                            let now = std::time::Instant::now();

                            match process_one_changeset(
                                &cs_id,
                                &ctx,
                                repo,
                                log_to_ods,
                                &bookmark_name,
                            )
                            .await
                            {
                                Ok(res) => {
                                    stat::log_changeset_done(&ctx, &cs_id, now.elapsed());
                                    Ok(res)
                                }
                                Err(e) => {
                                    stat::log_changeset_error(&ctx, &cs_id, &e, now.elapsed());
                                    Err(e)
                                }
                            }
                        })
                    })
                    .buffered(config.changeset_concurrency as usize)
                    .map_err(anyhow::Error::from)
                    .try_next_step(|messages| {
                        cloned!(mut send_manager);
                        async move { send_messages_in_order(messages, &mut send_manager).await }
                    })
                    .try_collect::<()>()
                    .await?;

                *current_position.lock().await += ms_len as u64;
                send_manager
                    .send_changeset(ChangesetMessage::CheckpointInEntry {
                        position: current_position.lock().await.clone(),
                        entry_id,
                        counter_names: counter_names.clone(),
                    })
                    .await?;
                Ok(())
            }
        })
        .await?;

    send_manager
        .send_changeset(ChangesetMessage::CheckpointInEntry {
            position: 0,
            entry_id: entry.id.0 as i64,
            counter_names: counter_names.clone(),
        })
        .await?;

    for bookmark_entry in bookmark_entries {
        send_manager
            .send_changeset(ChangesetMessage::FinishEntry(BookmarkInfo {
                name: bookmark_entry.bookmark_name.name().to_string(),
                from_cs_id: bookmark_entry.from_changeset_id,
                to_cs_id: bookmark_entry.to_changeset_id,
                log_id: bookmark_entry.id.0 as i64,
                reason: bookmark_entry.reason,
                counter_names: counter_names.clone(),
            }))
            .await?;
    }

    // FIXME(acampi) Temporarily disable to fix stuck sync: https://fb.workplace.com/groups/1708850869939124/permalink/1994176751406533/
    // bul_util::update_remaining_moves(
    //     entry.id,
    //     repo_name.clone(),
    //     ctx.clone(),
    //     repo.bookmark_update_log_arc(),
    // )
    // .await?;

    Ok(())
}

pub async fn process_one_changeset(
    cs_id: &ChangesetId,
    ctx: &CoreContext,
    repo: Repo,
    log_to_ods: bool,
    bookmark_name: &str,
) -> Result<Messages> {
    stat::log_changeset_start(ctx, cs_id);

    let mut messages = Messages::default();

    let now = std::time::Instant::now();

    let (cs_info, hg_cs_id, bs_cs) = futures::try_join!(
        async {
            anyhow::Ok(
                repo.repo_derived_data()
                    .derive::<ChangesetInfo>(ctx, cs_id.clone(), DerivationPriority::LOW)
                    .await?,
            )
        },
        async { anyhow::Ok(repo.derive_hg_changeset(ctx, *cs_id).await?) },
        async { anyhow::Ok(cs_id.load(ctx, repo.repo_blobstore()).await?) },
    )?;
    let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    let commit_time = bs_cs.author_date().timestamp_secs();
    let cids: Vec<_> = bs_cs
        .file_changes()
        .filter_map(|(_path, file_change)| match file_change {
            FileChange::Change(change) => Some(change.content_id()),
            FileChange::UntrackedChange(change) => Some(change.content_id()),
            FileChange::Deletion | FileChange::UntrackedDeletion => None,
        })
        .collect();

    if cids.is_empty() && hg_cs.manifestid().as_bytes() == [0; SHA1_HASH_LENGTH_BYTES] {
        tracing::info!("Changeset {} has no content", cs_id);
    } else {
        process_one_changeset_contents(ctx, &mut messages, cs_info, &repo, &hg_cs, cids).await?;
    }

    messages
        .changeset_messages
        .push(ChangesetMessage::Changeset((hg_cs, bs_cs)));

    if log_to_ods {
        let lag = if let Some(cs_id) = repo
            .bookmarks()
            .get(
                ctx.clone(),
                &BookmarkKey::new(bookmark_name)?,
                bookmarks::Freshness::MaybeStale,
            )
            .await?
        {
            let bookmark_commit = cs_id.load(ctx, repo.repo_blobstore()).await?;
            let bookmark_commit_time = bookmark_commit.author_date().timestamp_secs();

            Some(bookmark_commit_time - commit_time)
        } else {
            tracing::info!("Bookmark {} not found", bookmark_name);
            None
        };

        messages.changeset_messages.push(ChangesetMessage::Log((
            repo.repo_identity().name().to_string(),
            lag,
        )));
    }

    let elapsed = now.elapsed();
    STATS::changeset_processed_time_ms.add_value(
        elapsed.as_millis() as i64,
        (repo.repo_identity().name().to_string(),),
    );
    STATS::changeset_processed_count.add_value(1, (repo.repo_identity().name().to_string(),));

    Ok(messages)
}

pub async fn process_one_changeset_contents(
    ctx: &CoreContext,
    messages: &mut Messages,
    cs_info: ChangesetInfo,
    repo: &Repo,
    hg_cs: &HgBlobChangeset,
    cids: Vec<ContentId>,
) -> Result<()> {
    // Read the sizes of the contents concurrently (by reading the metadata blobs from blobstore)
    // Larger commits/older not cached commits would benefit from this concurrency.
    let mut contents = stream::iter(cids)
        .map(|cid| {
            cloned!(ctx, repo);
            async move {
                let metadata =
                    filestore::get_metadata(repo.repo_blobstore(), &ctx, &FetchKey::Canonical(cid))
                        .await?
                        .expect("blob not found");

                anyhow::Ok(ContentMessage::Content(cid, metadata.total_size))
            }
        })
        .buffered(100)
        .try_collect::<Vec<_>>()
        .await?;

    messages.content_messages.append(&mut contents);

    // Notify contents for this changeset are ready
    let (content_files_tx, content_files_rx) = oneshot::channel();
    let (content_trees_tx, content_trees_rx) = oneshot::channel();
    messages.content_messages.push(ContentMessage::ContentDone(
        content_files_tx,
        content_trees_tx,
    ));

    let parents = cs_info.parents().collect::<Vec<_>>();
    let mf_ids_p = future::try_join_all(parents.iter().map(|parent| {
        cloned!(ctx, repo);
        async move {
            let hg_cs_id = repo.derive_hg_changeset(&ctx, *parent).await?;
            let hg_cs = hg_cs_id.load(&ctx, repo.repo_blobstore()).await?;
            let hg_mf_id = hg_cs.manifestid();
            anyhow::Ok::<HgManifestId>(hg_mf_id)
        }
    }))
    .await?;

    let hg_mf_id = hg_cs.manifestid();
    let (mut mf_ids, file_ids) = sort_manifest_changes(ctx, repo, hg_mf_id, mf_ids_p).await?;
    mf_ids.push(hg_mf_id);

    // Send files and trees
    messages
        .files_messages
        .push(FileMessage::WaitForContents(content_files_rx));

    messages
        .trees_messages
        .push(TreeMessage::WaitForContents(content_trees_rx));

    // Notify files and trees for this changeset are ready
    let (f_tx, f_rx) = oneshot::channel();
    let (t_tx, t_rx) = oneshot::channel();

    let (_, _) = tokio::try_join!(
        async {
            messages
                .trees_messages
                .extend(mf_ids.into_iter().map(TreeMessage::Tree));
            messages.trees_messages.push(TreeMessage::TreesDone(t_tx));
            anyhow::Ok(())
        },
        async {
            messages
                .files_messages
                .extend(file_ids.into_iter().map(FileMessage::FileNode));
            messages.files_messages.push(FileMessage::FilesDone(f_tx));
            anyhow::Ok(())
        }
    )?;

    // Upload changeset
    messages
        .changeset_messages
        .push(ChangesetMessage::WaitForFilesAndTrees(f_rx, t_rx));

    Ok(())
}

async fn sort_manifest_changes(
    ctx: &CoreContext,
    repo: &Repo,
    mf_id: HgManifestId,
    mf_ids_p: Vec<HgManifestId>,
) -> Result<(Vec<mercurial_types::HgManifestId>, Vec<HgFileNodeId>)> {
    let mut mf_ids: Vec<mercurial_types::HgManifestId> = vec![];
    let mut file_ids: Vec<HgFileNodeId> = vec![];

    let entries = find_intersection_of_hg_diffs(
        ctx,
        repo.repo_blobstore().clone(),
        repo.repo_identity().name(),
        mf_id,
        mf_ids_p,
    );
    futures::pin_mut!(entries);

    while let Some((path, entry)) = entries.try_next().await? {
        match entry {
            // The caller appends the root manifest itself.
            Entry::Tree(_) if path.is_root() => {}
            Entry::Tree(mf_id) => mf_ids.push(mf_id),
            Entry::Leaf((_ftype, nodeid)) => file_ids.push(nodeid),
        }
    }

    Ok((mf_ids, file_ids))
}

pub async fn send_messages_in_order(
    messages: Result<Messages>,
    send_manager: &mut SendManager,
) -> Result<()> {
    let messages = messages?;

    send_manager
        .send_contents(messages.content_messages)
        .await?;

    let (_, _) = tokio::try_join!(
        async {
            send_manager.send_files(messages.files_messages).await?;
            anyhow::Ok(())
        },
        async {
            send_manager.send_trees(messages.trees_messages).await?;
            anyhow::Ok(())
        }
    )?;

    send_manager
        .send_changesets(messages.changeset_messages)
        .await?;

    Ok(())
}

pub(crate) async fn get_unsharded_repo_args(
    app: Arc<MononokeApp>,
    sync_args: &SyncArgs,
) -> Result<(SourceRepoArgs, String, String)> {
    let source_repo: Repo = app.open_repo(&sync_args.repo).await?;
    let source_repo_name = source_repo.repo_identity.name().to_string();
    let target_repo_name = sync_args
        .dest_repo_name
        .clone()
        .unwrap_or(source_repo_name.clone());

    Ok((
        SourceRepoArgs::with_name(source_repo_name.clone()),
        source_repo_name,
        target_repo_name,
    ))
}
