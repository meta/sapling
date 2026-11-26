/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use bookmarks::BookmarkKey;
use context::CoreContext;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::RepoConfigRef;
use mononoke_macros::mononoke;
use mononoke_types::CaseConflictTrie;
use mononoke_types::PrefixTrie;
use pushrebase_hooks::RepoLockPushrebaseHook;
use pushrebase_hooks::get_pushrebase_hooks;
use shared_error::std::SharedError;
use stats::prelude::*;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::PushrebaseError;
use crate::PushrebaseOutcome;
use crate::PushrebaseQueueRepo;
use crate::PushrebaseRetryNum;
use crate::PushrebaseStack;
use crate::RepoLockPolicy;
use crate::do_batched_pushrebase;
use crate::pushrebase_context;

/// A pushrebase request ready to be queued.
pub struct PushrebaseRequest {
    pub ctx: CoreContext,
    pub stack: PushrebaseStack,
    pub flags: PushrebaseFlags,
    pub repo_lock: RepoLockPolicy,
    pub response_tx: oneshot::Sender<Result<PushrebaseOutcome, SharedError<PushrebaseError>>>,
    pub enqueued_at: tokio::time::Instant,
}

define_stats! {
    prefix = "mononoke.land_service.batcher";
    batch_size: timeseries(Average, Sum, Count),
    batch_total_commits: timeseries(Average, Sum, Count),
    batch_queue_time_ms: quantile_stat(Average, Sum, Count; P 50, P 90, P 99; Duration::from_secs(60), Duration::from_secs(600), Duration::from_secs(3600)),
    intra_batch_conflict_count: timeseries(Rate, Sum),
}

mod service_stats {
    use stats::prelude::*;

    define_stats! {
        prefix = "mononoke.land_service";
        total_batch_success: timeseries(Rate, Sum),
        total_batch_failures: timeseries(Rate, Sum),
    }

    pub(super) fn record(success: bool) {
        if success {
            STATS::total_batch_success.add_value(1);
        } else {
            STATS::total_batch_failures.add_value(1);
        }
    }
}

pub(super) struct QueuedPushrebaseRequest {
    pub(super) ctx: CoreContext,
    pub(super) stack: PushrebaseStack,
    pub(super) retry_num: PushrebaseRetryNum,
    pub(super) response_tx:
        oneshot::Sender<Result<PushrebaseOutcome, SharedError<PushrebaseError>>>,
    pub(super) enqueued_at: tokio::time::Instant,
}

pub(super) struct PushrebaseRequestBatch {
    pub(super) flags: PushrebaseFlags,
    pub(super) repo_lock: RepoLockPolicy,
    pub(super) requests: Vec<QueuedPushrebaseRequest>,
}

/// Serializes and batches pushrebases for one repository bookmark.
#[derive(Clone)]
pub struct PushrebaseQueue {
    sender: mpsc::Sender<PushrebaseRequest>,
}

impl PushrebaseQueue {
    pub fn new<R, F>(
        batch_ctx: CoreContext,
        repo_name: String,
        bookmark: BookmarkKey,
        resolve_repo: F,
    ) -> Self
    where
        R: PushrebaseQueueRepo + 'static,
        F: Fn() -> Option<Arc<R>> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel(1000);
        mononoke::spawn_task(run_queue(
            batch_ctx,
            repo_name,
            bookmark,
            resolve_repo,
            receiver,
        ));
        Self { sender }
    }

    /// Enqueues a request, reporting queue failure through its response channel.
    pub async fn enqueue(&self, request: PushrebaseRequest) {
        if let Err(error) = self.sender.send(request).await {
            let _ = error
                .0
                .response_tx
                .send(Err(SharedError::from(PushrebaseError::Error(anyhow!(
                    "Pushrebase request queue is closed"
                )))));
        }
    }
}

fn partition_requests(
    retry_batches: Vec<PushrebaseRequestBatch>,
    requests: Vec<PushrebaseRequest>,
) -> (Vec<PushrebaseRequestBatch>, usize) {
    let mut batches = vec![];
    let mut conflicts = 0;
    let request_batches = requests.into_iter().map(|request| {
        let ctx = pushrebase_context(&request.ctx, &request.flags);
        let mut flags = request.flags;
        // Attribution belongs to each request, not to the shared execution.
        flags.land_instance_id = None;
        flags.phab_diff_id = None;
        PushrebaseRequestBatch {
            flags,
            repo_lock: request.repo_lock,
            requests: vec![QueuedPushrebaseRequest {
                ctx,
                stack: request.stack,
                retry_num: PushrebaseRetryNum(0),
                response_tx: request.response_tx,
                enqueued_at: request.enqueued_at,
            }],
        }
    });
    let mut request_batches = retry_batches
        .into_iter()
        .chain(request_batches)
        .map(|batch| {
            // Merges may derive the onto manifest, so they need a persisted base.
            let has_merge = batch
                .requests
                .iter()
                .flat_map(|request| request.stack.changesets.iter())
                .any(|changeset| changeset.is_merge());
            (batch, has_merge)
        })
        .peekable();

    while let Some((mut batch, has_merge)) = request_batches.next() {
        // Rebase mappings and hooks are keyed by the original changeset ID.
        let mut changeset_ids = batch
            .requests
            .iter()
            .flat_map(|request| request.stack.changesets.iter())
            .map(|changeset| changeset.get_changeset_id())
            .collect::<HashSet<_>>();
        let mut changed_files = batch
            .requests
            .iter()
            .flat_map(|request| request.stack.changed_files.iter().cloned())
            .collect::<PrefixTrie>();
        {
            let mut case_conflicts =
                CaseConflictTrie::new(&batch.flags.casefolding_check_excluded_paths);
            let has_case_conflict = batch.flags.casefolding_check
                && case_conflicts
                    .check_conflicts(
                        batch
                            .requests
                            .iter()
                            .flat_map(|request| request.stack.changesets.iter()),
                    )
                    .is_some();

            while !has_merge
                && let Some((request_batch, false)) = request_batches.peek_mut()
                && batch.flags == request_batch.flags
                && batch.repo_lock == request_batch.repo_lock
            {
                let request_changed_files = request_batch
                    .requests
                    .iter()
                    .flat_map(|request| request.stack.changed_files.iter().cloned())
                    .collect::<Vec<_>>();
                if request_changed_files
                    .iter()
                    .any(|path| changed_files.has_path_conflict(path))
                    || request_batch
                        .requests
                        .iter()
                        .flat_map(|request| request.stack.changesets.iter())
                        .any(|changeset| !changeset_ids.insert(changeset.get_changeset_id()))
                    || (batch.flags.casefolding_check
                        && (has_case_conflict
                            || case_conflicts
                                .check_conflicts(
                                    request_batch
                                        .requests
                                        .iter()
                                        .flat_map(|request| request.stack.changesets.iter()),
                                )
                                .is_some()))
                {
                    conflicts += 1;
                    break;
                }

                changed_files.extend(request_changed_files);
                batch.requests.append(&mut request_batch.requests);
                let _ = request_batches.next();
            }
        }
        batches.push(batch);
    }

    (batches, conflicts)
}

async fn run_queue<R, F>(
    batch_ctx: CoreContext,
    repo_name: String,
    bookmark: BookmarkKey,
    resolve_repo: F,
    mut receiver: mpsc::Receiver<PushrebaseRequest>,
) where
    R: PushrebaseQueueRepo + 'static,
    F: Fn() -> Option<Arc<R>>,
{
    let mut retries: VecDeque<PushrebaseRequestBatch> = VecDeque::new();
    let mut receiver_closed = false;

    loop {
        let mut retry_batches = vec![];
        let mut requests = vec![];
        let mut total_commits = match retries.pop_front() {
            Some(batch) => {
                let commit_count = batch
                    .requests
                    .iter()
                    .map(|request| request.stack.changesets.len())
                    .sum::<usize>();
                retry_batches.push(batch);
                commit_count
            }
            None if receiver_closed => return,
            None => match receiver.recv().await {
                Some(request) => {
                    let commit_count = request.stack.changesets.len();
                    requests.push(request);
                    commit_count
                }
                None => return,
            },
        };
        let max_batch_time = Duration::from_millis(justknobs::get_as::<u64>(
            "scm/mononoke:land_service_batch_time_ms",
            Some(&repo_name),
        ));
        let max_batch_commits = justknobs::get_as::<usize>(
            "scm/mononoke:land_service_batch_max_commits",
            Some(&repo_name),
        );
        let sleep = tokio::time::sleep(max_batch_time);
        tokio::pin!(sleep);

        while total_commits < max_batch_commits {
            if let Some(batch) = retries.pop_front() {
                total_commits += batch
                    .requests
                    .iter()
                    .map(|request| request.stack.changesets.len())
                    .sum::<usize>();
                retry_batches.push(batch);
                continue;
            }
            if receiver_closed {
                break;
            }

            tokio::select! {
                _ = &mut sleep => break,
                request = receiver.recv() => match request {
                    Some(request) => {
                        total_commits += request.stack.changesets.len();
                        requests.push(request);
                    }
                    None => {
                        receiver_closed = true;
                        break;
                    }
                }
            }
        }

        let (batches, conflicts) = partition_requests(retry_batches, requests);
        STATS::intra_batch_conflict_count.add_value(conflicts as i64);

        for mut batch in batches {
            let batch_total_commits = batch
                .requests
                .iter()
                .map(|request| request.stack.changesets.len())
                .sum::<usize>();
            STATS::batch_size.add_value(batch.requests.len() as i64);
            STATS::batch_total_commits.add_value(batch_total_commits as i64);

            let max_requeue = justknobs::get_as::<usize>(
                "scm/mononoke:land_service_batch_max_requeue",
                Some(&repo_name),
            );
            let Some(repo) = resolve_repo() else {
                service_stats::record(false);
                let error = SharedError::from(PushrebaseError::Error(anyhow!(
                    "Pushrebase repository is not loaded"
                )));
                for request in batch.requests {
                    let _ = request.response_tx.send(Err(error.clone()));
                }
                continue;
            };
            let ctx = batch_ctx.clone_and_reset().with_mutated_scuba(|mut scuba| {
                scuba
                    .add("repo_name", repo.repo_identity().name())
                    .add("bookmark", bookmark.to_string())
                    .add("batch_size", batch.requests.len())
                    .add("batch_total_commits", batch_total_commits);
                scuba
            });
            let batch_size = batch.requests.len();
            for request in &mut batch.requests {
                let queue_time_ms = request
                    .enqueued_at
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(i64::MAX);
                STATS::batch_queue_time_ms.add_value(queue_time_ms);
                request.ctx = request.ctx.with_mutated_scuba(|mut scuba| {
                    scuba
                        .add("repo_name", repo.repo_identity().name())
                        .add("bookmark", bookmark.as_str())
                        .add("changesets_count", request.stack.changesets.len());
                    scuba
                });
                request
                    .ctx
                    .scuba()
                    .clone()
                    .add("batch_size", batch_size)
                    .add("batch_total_commits", batch_total_commits)
                    .add("batch_queue_time_ms", queue_time_ms)
                    .add("retry_num", request.retry_num.0)
                    .log_with_msg("Batch received", None);
            }
            let mut hooks = match get_pushrebase_hooks(
                &ctx,
                repo.as_ref(),
                &bookmark,
                &repo.repo_config().pushrebase,
                None,
            )
            .await
            {
                Ok(hooks) => hooks,
                Err(error) => {
                    service_stats::record(false);
                    let error = SharedError::from(PushrebaseError::Error(error.into()));
                    for request in batch.requests {
                        let _ = request.response_tx.send(Err(error.clone()));
                    }
                    continue;
                }
            };
            if batch.repo_lock == RepoLockPolicy::Enforce {
                hooks.push(RepoLockPushrebaseHook::new(repo.repo_identity().id()));
            }

            let requests = std::mem::take(&mut batch.requests);
            let failures = do_batched_pushrebase(
                &ctx,
                repo.as_ref(),
                &batch.flags,
                &bookmark,
                requests,
                &hooks,
            )
            .await;
            service_stats::record(failures.is_empty());
            let mut retry_requests = vec![];
            for request in failures {
                if request.retry_num.0 < max_requeue {
                    retry_requests.push(request);
                    continue;
                }

                if batch.flags.monitoring_bookmark.is_some() {
                    bookmarks::saturation::record_pushrebase_retries(
                        repo.repo_identity().name(),
                        request.retry_num.0 as i64,
                    );
                }
                let error = SharedError::from(PushrebaseError::Error(anyhow!(
                    "Exceeded maximum requeue attempts ({max_requeue})"
                )));
                let _ = request.response_tx.send(Err(error));
            }
            if !retry_requests.is_empty() {
                batch.requests = retry_requests;
                retries.push_back(batch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blobstore::Loadable;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use mononoke_types::BonsaiChangesetMut;
    use mononoke_types::ChangesetId;
    use mononoke_types::ContentId;
    use mononoke_types::FileChange;
    use mononoke_types::FileType;
    use mononoke_types::GitLfs;
    use mononoke_types::NonRootMPath;
    use mononoke_types::hash::Blake2;
    use repo_blobstore::RepoBlobstoreRef;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;

    use super::*;
    use crate::tests::PushrebaseTestRepo;

    fn request(fb: FacebookInit, path: &str) -> PushrebaseRequest {
        let id = ChangesetId::new(Blake2::from_byte_array([1; 32]));
        let path = NonRootMPath::new(path).expect("test path should be valid");
        let changeset = BonsaiChangesetMut {
            file_changes: [(
                path.clone(),
                FileChange::tracked(
                    ContentId::new(Blake2::from_byte_array([2; 32])),
                    FileType::Regular,
                    1,
                    None,
                    GitLfs::FullContent,
                ),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }
        .freeze()
        .expect("test changeset should be valid");
        let (response_tx, _) = oneshot::channel();
        PushrebaseRequest {
            ctx: CoreContext::test_mock(fb),
            stack: PushrebaseStack {
                changed_files: vec![path.into()],
                changesets: vec![changeset],
                head: id,
                root: id,
                required_root: None,
                conflict_check_base: id,
                carried_merge_file_info: vec![],
            },
            flags: PushrebaseFlags::default(),
            repo_lock: RepoLockPolicy::Bypass,
            response_tx,
            enqueued_at: tokio::time::Instant::now(),
        }
    }

    #[mononoke::fbinit_test]
    async fn partitions_conflicting_paths_without_reordering(fb: FacebookInit) {
        let (batches, conflicts) = partition_requests(
            vec![],
            vec![request(fb, "a"), request(fb, "a/b"), request(fb, "c")],
        );

        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.requests.len())
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(conflicts, 1);
    }

    #[mononoke::fbinit_test]
    async fn isolates_requests_with_merges(fb: FacebookInit) -> anyhow::Result<()> {
        for boundary in [true, false] {
            let mut merge = request(fb, "merge");
            let mut parent = merge.stack.changesets[0].clone().into_mut();
            parent.parents = vec![merge.stack.root];
            let parent = parent.freeze()?;
            let other_parent = if boundary {
                ChangesetId::new(Blake2::from_byte_array([3; 32]))
            } else {
                merge.stack.root
            };
            let head = BonsaiChangesetMut {
                parents: vec![parent.get_changeset_id(), other_parent],
                ..Default::default()
            }
            .freeze()?;
            merge.stack.head = head.get_changeset_id();
            merge.stack.changesets = vec![parent, head];
            let (batches, _) = partition_requests(
                vec![],
                vec![
                    request(fb, "a"),
                    request(fb, "b"),
                    merge,
                    request(fb, "c"),
                    request(fb, "d"),
                ],
            );
            assert_eq!(
                batches
                    .iter()
                    .map(|batch| batch.requests.len())
                    .collect::<Vec<_>>(),
                [2, 1, 2]
            );
            let (batches, _) = partition_requests(batches, vec![]);
            assert_eq!(
                batches
                    .iter()
                    .map(|batch| batch.requests.len())
                    .collect::<Vec<_>>(),
                [2, 1, 2],
                "retry batches must keep merge requests isolated"
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn shared_empty_commits_land_in_separate_batches(fb: FacebookInit) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
        let bookmark_name = BookmarkKey::new("master")?;
        let root = CreateCommitContext::new_root(&ctx, &repo).commit().await?;
        bookmark(&ctx, &repo, bookmark_name.clone())
            .set_to(root)
            .await?;
        let mut empty_commits = vec![];
        for message in ["first", "second"] {
            let id = CreateCommitContext::new(&ctx, &repo, vec![root])
                .set_message(message)
                .commit()
                .await?;
            empty_commits.push(id.load(&ctx, repo.repo_blobstore()).await?);
        }
        let (requests, receivers): (Vec<_>, Vec<_>) = [0, 1, 1, 0]
            .into_iter()
            .map(|index| {
                let (response_tx, response_rx) = oneshot::channel();
                let changeset = &empty_commits[index];
                (
                    PushrebaseRequest {
                        ctx: ctx.clone(),
                        stack: PushrebaseStack {
                            root,
                            required_root: None,
                            conflict_check_base: root,
                            carried_merge_file_info: vec![],
                            head: changeset.get_changeset_id(),
                            changesets: vec![changeset.clone()],
                            changed_files: vec![],
                        },
                        flags: PushrebaseFlags::default(),
                        repo_lock: RepoLockPolicy::Bypass,
                        response_tx,
                        enqueued_at: tokio::time::Instant::now(),
                    },
                    response_rx,
                )
            })
            .unzip();
        let (batches, conflicts) = partition_requests(vec![], requests);
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.requests.len())
                .collect::<Vec<_>>(),
            [2, 2]
        );
        assert_eq!(conflicts, 1);
        let (batches, conflicts) = partition_requests(batches, vec![]);
        assert_eq!(batches.len(), 2, "retry batches must remain separate");
        assert_eq!(conflicts, 1);
        for batch in batches {
            assert!(
                do_batched_pushrebase(
                    &ctx,
                    &repo,
                    &batch.flags,
                    &bookmark_name,
                    batch.requests,
                    &[]
                )
                .await
                .is_empty()
            );
        }
        for (receiver, index) in receivers.into_iter().zip([0, 1, 1, 0]) {
            let outcome = receiver.await??;
            assert_eq!(outcome.rebased_changesets.len(), 1);
            assert_eq!(
                outcome.rebased_changesets[0].id_old,
                empty_commits[index].get_changeset_id()
            );
            assert_eq!(outcome.rebased_changesets[0].id_new, outcome.head);
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn partitions_by_execution_inputs(fb: FacebookInit) {
        let mut first = request(fb, "a");
        first.flags.land_instance_id = Some("first land".to_owned());
        first.flags.phab_diff_id = Some("first diff".to_owned());
        let mut second = request(fb, "b");
        second.flags.land_instance_id = Some("second land".to_owned());
        second.flags.phab_diff_id = Some("second diff".to_owned());
        let mut different_flags = request(fb, "c");
        different_flags.flags.rewritedates = false;
        let mut enforcing = request(fb, "d");
        enforcing.repo_lock = RepoLockPolicy::Enforce;
        let mut same_enforcement = request(fb, "e");
        same_enforcement.repo_lock = RepoLockPolicy::Enforce;
        let (batches, conflicts) = partition_requests(
            vec![],
            vec![first, second, different_flags, enforcing, same_enforcement],
        );

        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.requests.len())
                .collect::<Vec<_>>(),
            [2, 1, 2]
        );
        assert_eq!(batches[0].repo_lock, RepoLockPolicy::Bypass);
        assert_eq!(batches[2].repo_lock, RepoLockPolicy::Enforce);
        assert_eq!(conflicts, 0);
    }

    #[mononoke::fbinit_test]
    async fn partitions_case_conflicts_when_enabled(fb: FacebookInit) {
        let mut upper = request(fb, "dir/File");
        upper.flags.casefolding_check = true;
        let mut lower = request(fb, "dir/file");
        lower.flags.casefolding_check = true;

        let (batches, conflicts) = partition_requests(vec![], vec![upper, lower]);

        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.requests.len())
                .collect::<Vec<_>>(),
            [1, 1]
        );
        assert_eq!(conflicts, 1);
    }
}
